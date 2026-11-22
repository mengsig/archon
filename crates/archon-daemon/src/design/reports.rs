//! What the stage agent says over MCP: `design_preview` and `design_note`.

use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::design::{
    AssetSource, DesignNote, DesignSession, FailReason, NoteKind, NoteStatus, ReplyAuthor,
    SessionState, Target, ThreadEntry, is_in_flight, validate_preview_url,
};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::protocol::{AgentNoteStatus, Reply};
use archon_design_browser as adb;
use archon_queue::store::NoteChange;

use super::assets::NewAsset;
use super::{DesignService, WorktreeRef, refuse};
use crate::context::DaemonContext;
use crate::worktree::design_git::{self, NoteGit, Shape, Tidy};

impl DesignService {
    /// The session whose agent `caller` is, while its stage attempt is live.
    async fn agent_session(
        &self,
        ctx: &DaemonContext,
        caller: Option<&AetherAgentId>,
    ) -> Result<DesignSession, ArchonError> {
        let caller = caller.ok_or_else(|| {
            ArchonError::Forbidden("design tools are for the stage's agent".into())
        })?;
        if let Some(found) = Self::live_session_of(ctx, caller).await? {
            return Ok(found);
        }
        // A Design stage whose agent runs without a session: a retry or a
        // resume whose start did not open one (or opened it for an earlier
        // attempt). Open it now, so the agent's first `design_preview` works.
        // A run or item that is ending had its sessions ended already: its
        // agent's late call opens nothing.
        let stage = {
            let store = ctx.store.lock().await;
            store
                .live_stage_runs()?
                .into_iter()
                .filter(|s| {
                    s.aether_agent.as_ref() == Some(caller)
                        && s.kind == Some(archon_core::catalog::StageKind::Design)
                        && s.state == archon_core::run::StageState::Running
                })
                .max_by_key(|s| s.attempt)
                .filter(|s| {
                    store.run(&s.run).is_ok_and(|run| {
                        !run.state.is_terminal()
                            && store
                                .item(&run.item)
                                .is_ok_and(|item| !item.state.is_terminal())
                    })
                })
        };
        if let Some(stage) = stage {
            tracing::info!(stage = %stage.id, attempt = stage.attempt, "design: opening the session of a running stage on its agent's call");
            crate::executor::design_started(ctx, &stage, caller).await;
            if let Some(found) = Self::live_session_of(ctx, caller).await? {
                return Ok(found);
            }
        }
        Err(ArchonError::Forbidden("stage attempt is not live".into()))
    }

    /// The newest open session whose agent is `caller`.
    async fn live_session_of(
        ctx: &DaemonContext,
        caller: &AetherAgentId,
    ) -> Result<Option<DesignSession>, ArchonError> {
        let store = ctx.store.lock().await;
        let live = store.design_sessions_in(&[
            SessionState::Starting,
            SessionState::Live,
            SessionState::Finishing,
            SessionState::Confirmed,
        ])?;
        // Oldest first: a resumed agent can hold an earlier attempt's
        // confirmed session still waiting on its gate; the newest attempt wins.
        Ok(live
            .into_iter()
            .filter(|s| s.agent.as_ref() == Some(caller))
            .max_by_key(|s| s.attempt))
    }

    pub(crate) async fn design_preview(
        &self,
        ctx: &DaemonContext,
        url: &str,
        caller: Option<&AetherAgentId>,
    ) -> Result<Reply, ArchonError> {
        let session = self.agent_session(ctx, caller).await?;
        let url = validate_preview_url(url)?;
        let became_live = {
            let mut store = ctx.store.lock().await;
            let patch = archon_queue::store::SessionPatch {
                preview_url: Some(Some(url.clone())),
                degraded: Some(false),
                ..Default::default()
            };
            store.design_session_patch(&session.id, patch)?;
            if session.state == SessionState::Starting {
                store.design_session_transition(
                    &session.id,
                    SessionState::Starting,
                    SessionState::Live,
                    None,
                    false,
                )?;
                let request = super::revise_note(&mut store, &session)?;
                Some((store.item(&session.item)?.title, request))
            } else {
                None
            }
        };
        let (became_live, request) = match became_live {
            Some((title, request)) => (Some(title), request),
            None => (None, None),
        };
        // The first report makes the session live: tell the operator, whose
        // inbox shows the entry with an Open button for the design screen.
        if let Some(title) = became_live {
            // The title is the operator's own words: clipped, so the inbox
            // entry and a push stay one short line.
            let title = crate::executor::clip(title.trim(), 80);
            let text =
                format!("Design session ready: {title}. Open it to show the agent what to change.");
            // The session waits on the operator: a Blocker is the kind that
            // asks for their action (Decision is a choice already made).
            if let Err(error) = crate::feed::notify(
                ctx,
                archon_core::notify::NotificationKind::Blocker,
                text,
                Some(session.item.clone()),
                Some(session.run.clone()),
            )
            .await
            {
                tracing::warn!(%error, session = %session.id, "design: the ready notice was not posted");
            }
        }
        let rt = self.runtime(&session.id);
        rt.st().last_url = Some(url.clone());
        let watched = rt.st().subscribers > 0;
        match rt.browser() {
            Some(browser) => {
                if let Err(error) = browser.navigate(&url).await {
                    tracing::info!(%error, "design: navigating the preview failed");
                }
            }
            // A screen opened before the app was up (a Revise's next session,
            // followed from the old one) subscribed while there was nothing
            // to show: start its preview now, not on its next reopen.
            None if watched => self.spawn_first_look(&session.id),
            None => {}
        }
        tracing::info!(session = %session.id, url, "design: preview reported");
        self.emit_session(ctx, &session.id).await;
        if let Some(note) = request {
            tracing::info!(note = %note.id, session = %session.id, "design: the operator's change request is the session's first note");
            self.emit_note(ctx, &session.item, &note.id).await;
            self.pump(ctx, &session.item).await?;
        }
        Ok(Reply::DesignDone)
    }

    /// Start the browser and frames of a session someone already watches,
    /// off the agent's request: a launch takes seconds.
    fn spawn_first_look(&self, session: &archon_core::design::DesignSessionId) {
        let Some(arc) = self.arc() else {
            return;
        };
        let session = session.clone();
        tokio::spawn(async move {
            if let Err(error) = arc.design.subscriber_joined(&arc, &session).await {
                tracing::warn!(%error, %session, "design: the watched preview did not start");
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn note_report(
        &self,
        ctx: &DaemonContext,
        id: &str,
        status: AgentNoteStatus,
        reply: Option<String>,
        commit: Option<String>,
        no_change: bool,
        path: Option<String>,
        caller: Option<&AetherAgentId>,
    ) -> Result<Reply, ArchonError> {
        if let Some(path) = &path {
            archon_core::design::check_app_path(path)
                .map_err(|why| ArchonError::Invalid(format!("path {why}")))?;
            if status != AgentNoteStatus::Done {
                return Err(ArchonError::Invalid(
                    "path goes with done: the page that shows the change".into(),
                ));
            }
        }
        let session = self.agent_session(ctx, caller).await?;
        let lock = self.item_lock(&session.item);
        let held = lock.lock().await;
        let result = self
            .report_locked(ctx, &session, id, status, reply, commit, no_change, path)
            .await;
        drop(held);
        if result.is_ok() {
            self.pump(ctx, &session.item).await?;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn report_locked(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        id: &str,
        status: AgentNoteStatus,
        reply: Option<String>,
        commit: Option<String>,
        no_change: bool,
        path: Option<String>,
    ) -> Result<Reply, ArchonError> {
        let item = &session.item;
        let note = ctx
            .store
            .lock()
            .await
            .design_note(item, id)?
            .ok_or_else(|| refuse(DesignRefusal::NotYours, format!("{id} is not yours")))?;
        // Only the current session's change or page note that is in flight.
        let yours = note.session == session.id
            && note.kind != NoteKind::Revert
            && is_in_flight(note.status);
        if !yours {
            return Err(refuse(
                DesignRefusal::NotYours,
                format!("{id} is not yours"),
            ));
        }
        let reply_text = reply
            .as_deref()
            .map(|r| archon_core::design::clean_text(r, archon_core::design::MAX_REPLY_CHARS))
            .filter(|r| !r.is_empty());
        if status == AgentNoteStatus::Working {
            let mut store = ctx.store.lock().await;
            if let Some(text) = reply_text {
                store.design_note_change(
                    item,
                    id,
                    NoteChange::Thread(thread(ReplyAuthor::Agent, text)),
                )?;
            }
            if note.status != NoteStatus::Working {
                store.design_note_change(
                    item,
                    id,
                    super::status_change(NoteStatus::Working, None),
                )?;
            }
            drop(store);
            self.emit_note(ctx, item, id).await;
            return self.note_reply(ctx, item, id).await;
        }
        let base = note
            .base_commit
            .clone()
            .ok_or_else(|| ArchonError::Corrupt(format!("{id} was sent without a base")))?;
        let wt = self.worktree_of(ctx, session).await?;
        let shadow = self.shadow(&wt)?;
        let g = NoteGit {
            shadow: &shadow,
            repo: &wt.repo,
            worktree: &wt.worktree,
            branch: &wt.branch,
        };
        let done_commit = (status == AgentNoteStatus::Done && !no_change)
            .then(|| commit.clone())
            .flatten();
        if status == AgentNoteStatus::Done && !no_change && done_commit.is_none() {
            return Err(ArchonError::Invalid(
                "done needs the commit you made".into(),
            ));
        }
        // History first (terminal), then tidiness (retryable).
        match design_git::check_shape(&g, &base, done_commit.as_deref())? {
            Shape::Accepted => {}
            Shape::HistoryChanged(why) => {
                tracing::info!(note = id, why, "design: history changed");
                {
                    let mut store = ctx.store.lock().await;
                    let text = "The code history changed in a way Archon can't track; its edits may still show; add a note if something should change.".to_owned();
                    store.design_note_change(
                        item,
                        id,
                        NoteChange::Thread(thread(ReplyAuthor::Archon, text)),
                    )?;
                    store.design_note_change(
                        item,
                        id,
                        super::status_change(NoteStatus::Failed, Some(FailReason::HistoryChanged)),
                    )?;
                }
                self.emit_note(ctx, item, id).await;
                return Ok(Reply::DesignAgent {
                    message: format!("{id} not recorded: history changed"),
                });
            }
        }
        let trailer = done_commit.as_ref().map(|_| id.to_owned());
        if let Tidy::Untidy(why) = design_git::check_tidy(&g, trailer.as_deref())? {
            return Err(ArchonError::Invalid(why));
        }
        let (to, fail) = match status {
            AgentNoteStatus::Done => (NoteStatus::Done, None),
            AgentNoteStatus::NeedsReply => (NoteStatus::NeedsReply, None),
            AgentNoteStatus::Failed => (NoteStatus::Failed, Some(FailReason::Agent)),
            AgentNoteStatus::Cancelled => (NoteStatus::Cancelled, None),
            AgentNoteStatus::Working => unreachable!("handled above"),
        };
        {
            let mut store = ctx.store.lock().await;
            if let Some(text) = reply_text {
                store.design_note_change(
                    item,
                    id,
                    NoteChange::Thread(thread(ReplyAuthor::Agent, text)),
                )?;
            }
            store.design_note_change(
                item,
                id,
                NoteChange::Status {
                    to,
                    fail_reason: fail,
                    commit: done_commit.clone(),
                    no_change: no_change && status == AgentNoteStatus::Done,
                },
            )?;
        }
        tracing::info!(note = id, ?status, device = ?super::current_device_label(), "design: agent reported");
        self.emit_note(ctx, item, id).await;
        if to == NoteStatus::Done && !no_change {
            // A page note's own page (a new screen): an element's note stays
            // on the page its element is on.
            let shown = path.filter(|_| note.target.is_none());
            self.after_picture(ctx, session, &note, &wt, shown.as_deref())
                .await;
            self.emit_note(ctx, item, id).await;
        }
        self.note_reply(ctx, item, id).await
    }

    /// The operator-only picture after a change: network idle, one reload when
    /// nothing moved, the same clip at the element's new box. A page note
    /// done with a `path` first takes the preview there and is pictured whole;
    /// its after picture records that path for the final picture.
    pub(crate) async fn after_picture(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        wt: &WorktreeRef,
        path: Option<&str>,
    ) {
        let Some(rt) = self.existing_runtime(&session.id) else {
            return;
        };
        let Some(browser) = rt.browser() else {
            return;
        };
        let shown = match (path, session.preview_url.as_deref()) {
            (Some(path), Some(url)) => {
                let to = format!("{}{path}", super::finish::origin_of(url));
                if let Err(error) = browser.navigate(&to).await {
                    tracing::info!(%error, "design: the preview did not go to the note's page");
                }
                Some(page_target(path, note))
            }
            _ => None,
        };
        let since = browser.latest_seq();
        let _ = browser
            .wait_network_idle(Duration::from_millis(500), Duration::from_secs(8))
            .await;
        let streaming = rt.st().screencast;
        if streaming
            && !browser
                .frame_changed_within(since, Duration::from_secs(8))
                .await
        {
            let _ = browser.reload().await;
            let _ = browser
                .wait_network_idle(Duration::from_millis(500), Duration::from_secs(8))
                .await;
        }
        let clip = match note.target.as_ref().and_then(|t| t.selector.as_deref()) {
            Some(selector) => browser
                .resolve_selector(
                    selector,
                    note.target.as_ref().map(super::finish::recorded_box),
                )
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let picture = match browser.capture(clip, 2.0).await {
            Ok(picture) => picture,
            Err(error) => {
                tracing::info!(%error, "design: the after picture was not taken");
                return;
            }
        };
        let head = self.head_of(wt).unwrap_or_default();
        let target = shown.or_else(|| note.target.clone());
        if let Err(error) = self.keep_after(ctx, note, target, &picture, &head).await {
            tracing::info!(%error, "design: the after picture was not kept");
        }
    }

    async fn keep_after(
        &self,
        ctx: &DaemonContext,
        note: &DesignNote,
        target: Option<Target>,
        picture: &adb::Picture,
        head: &str,
    ) -> Result<(), ArchonError> {
        let asset = self
            .put_asset(
                ctx,
                &note.item,
                NewAsset {
                    source: AssetSource::After,
                    target,
                    note: Some(note.id.clone()),
                    commit: Some(head.to_owned()),
                    mime: picture.mime,
                    bytes: &picture.bytes,
                    width: picture.width,
                    height: picture.height,
                    sketch: None,
                },
            )
            .await?;
        if let Some(asset) = asset {
            ctx.store.lock().await.design_note_change(
                &note.item,
                &note.id,
                NoteChange::After {
                    asset: asset.id,
                    captured_commit: head.to_owned(),
                },
            )?;
        }
        Ok(())
    }
}

/// Where a page note's change shows: the whole view at `path`, at the
/// note's viewport. The after picture records it; Finish's final picture
/// for the note is taken there.
pub(crate) fn page_target(path: &str, note: &DesignNote) -> Target {
    let v = note.viewport;
    Target {
        path: path.to_owned(),
        selector: None,
        role_name: None,
        component: None,
        rect: archon_core::design::Rect {
            x: 0.0,
            y: 0.0,
            w: f64::from(v.w),
            h: f64::from(v.h),
        },
        viewport: v,
        styles: Default::default(),
        region: false,
    }
}

fn thread(from: ReplyAuthor, text: String) -> ThreadEntry {
    ThreadEntry {
        at: archon_core::now(),
        from,
        text,
    }
}
