//! What the stage agent says over MCP: `design_preview` and `design_note`.

use archon_core::aether_ids::AetherAgentId;
use archon_core::design::{
    AssetSource, DesignNote, DesignSession, FailReason, NO_VISIBLE_CHANGE, NoteKind, NoteStatus,
    RETAKE_ASKED, ReplyAuthor, SessionState, Target, ThreadEntry, UiStep, is_in_flight,
    validate_preview_url,
};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::protocol::{AgentNoteStatus, Reply};
use archon_design_browser as adb;
use archon_queue::store::NoteChange;

use super::assets::NewAsset;
use super::state::{AfterOutcome, AfterShot};
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
        let (became_live, request) = {
            let mut store = ctx.store.lock().await;
            let patch = archon_queue::store::SessionPatch {
                preview_url: Some(Some(url.clone())),
                degraded: Some(false),
                ..Default::default()
            };
            store.design_session_patch(&session.id, patch)?;
            let became_live = if session.state == SessionState::Starting {
                store.design_session_transition(
                    &session.id,
                    SessionState::Starting,
                    SessionState::Live,
                    None,
                    false,
                )?;
                Some(store.item(&session.item)?.title)
            } else {
                None
            };
            // On every report while live, not only the first: adding it is
            // idempotent, so a request a failure kept out arrives on the
            // agent's next report (or the next pump) instead of being lost;
            // and a failure here never fails the report that made the session
            // live.
            let request = match session.state {
                SessionState::Starting | SessionState::Live => {
                    super::revise_note(&mut store, &session).unwrap_or_else(|error| {
                        tracing::warn!(%error, session = %session.id, "design: the operator's change request was not added; the next preview report tries again");
                        None
                    })
                }
                _ => None,
            };
            (became_live, request)
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
            // The note is stored queued: a failed delivery here is retried
            // by the next pump or sweep, so it never fails the agent's report.
            if let Err(error) = self.pump(ctx, &session.item).await {
                tracing::warn!(%error, session = %session.id, "design: delivering the operator's change request failed");
            }
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

    pub(crate) async fn note_report(
        &self,
        ctx: &DaemonContext,
        report: NoteReport,
        caller: Option<&AetherAgentId>,
    ) -> Result<Reply, ArchonError> {
        check_report(&report)?;
        let session = self.agent_session(ctx, caller).await?;
        let lock = self.item_lock(&session.item);
        let held = lock.lock().await;
        let result = self.report_locked(ctx, &session, report).await;
        drop(held);
        if result.is_ok() {
            self.pump(ctx, &session.item).await?;
        }
        result
    }

    async fn report_locked(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        report: NoteReport,
    ) -> Result<Reply, ArchonError> {
        let item = &session.item;
        let id = report.id.as_str();
        let (note, all) = {
            let store = ctx.store.lock().await;
            (store.design_note(item, id)?, store.design_notes(item)?)
        };
        let note = note.ok_or_else(|| {
            refuse(
                DesignRefusal::NotYours,
                format!("{id} is not a note of this item"),
            )
        })?;
        let reply_text = report
            .reply
            .as_deref()
            .map(|r| archon_core::design::clean_text(r, archon_core::design::MAX_REPLY_CHARS))
            .filter(|r| !r.is_empty());
        match standing(&note, session, &all, &report)? {
            Standing::Retake => self.retake(ctx, session, &note, &report, reply_text).await,
            Standing::InFlight if report.status == AgentNoteStatus::Working => {
                self.report_working(ctx, session, &note, &report, reply_text)
                    .await
            }
            Standing::InFlight => {
                self.report_settled(ctx, session, &note, report, reply_text)
                    .await
            }
        }
    }

    /// `working`: the note is being worked on; with steps, its before picture
    /// is retaken in the state they reach.
    async fn report_working(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        report: &NoteReport,
        reply_text: Option<String>,
    ) -> Result<Reply, ArchonError> {
        let (item, id) = (&session.item, report.id.as_str());
        {
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
        }
        let before = match report.steps.is_empty() {
            true => Ok(()),
            false => {
                self.before_in_state(ctx, session, note, &report.steps)
                    .await
            }
        };
        self.emit_note(ctx, item, id).await;
        match before {
            Ok(()) => self.note_reply(ctx, item, id).await,
            Err(why) => Ok(Reply::DesignAgent {
                message: format!(
                    "{id} is working, but its before picture was not taken in that state: {why}. \
                     Fix the steps and report working again before you change the code."
                ),
            }),
        }
    }

    /// `done`, `needs_reply`, `failed` or `cancelled` on a note in flight.
    async fn report_settled(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        report: NoteReport,
        reply_text: Option<String>,
    ) -> Result<Reply, ArchonError> {
        let (item, id) = (&session.item, report.id.as_str());
        let status = report.status;
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
        let done_commit = (status == AgentNoteStatus::Done && !report.no_change)
            .then(|| report.commit.clone())
            .flatten();
        if status == AgentNoteStatus::Done && !report.no_change && done_commit.is_none() {
            return Err(ArchonError::Invalid(
                "done needs the commit you made".into(),
            ));
        }
        // History first (terminal), then tidiness (retryable).
        match design_git::check_shape(&g, &base, done_commit.as_deref())? {
            Shape::Accepted => {}
            Shape::HistoryChanged(why) => return self.history_changed(ctx, item, id, why).await,
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
            AgentNoteStatus::Working => unreachable!("handled by the caller"),
        };
        {
            let mut store = ctx.store.lock().await;
            if let Some(text) = reply_text.clone() {
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
                    no_change: report.no_change && status == AgentNoteStatus::Done,
                },
            )?;
        }
        tracing::info!(note = id, ?status, device = ?super::current_device_label(), "design: agent reported");
        self.emit_note(ctx, item, id).await;
        if to == NoteStatus::Done && !report.no_change {
            let fresh = ctx
                .store
                .lock()
                .await
                .design_note(item, id)?
                .unwrap_or_else(|| note.clone());
            return self
                .picture_after_done(ctx, session, &fresh, &wt, &report, reply_text)
                .await;
        }
        self.note_reply(ctx, item, id).await
    }

    async fn history_changed(
        &self,
        ctx: &DaemonContext,
        item: &archon_core::id::QueueItemId,
        id: &str,
        why: String,
    ) -> Result<Reply, ArchonError> {
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
        Ok(Reply::DesignAgent {
            message: format!("{id} not recorded: history changed"),
        })
    }

    /// A done note's after picture, from the agent's `path` and `steps`.
    async fn picture_after_done(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        wt: &WorktreeRef,
        report: &NoteReport,
        reply_text: Option<String>,
    ) -> Result<Reply, ArchonError> {
        // A page note's own page (a new screen): an element's note stays on
        // the page its element is on.
        let shown = report.path.as_deref().filter(|_| note.target.is_none());
        let shot = AfterShot {
            path: shown,
            steps: &report.steps,
        };
        let outcome = self.after_picture(ctx, session, note, wt, shot).await;
        self.emit_note(ctx, &note.item, &note.id).await;
        self.settle_outcome(ctx, note, outcome, reply_text).await
    }

    /// A repeated `done` on a done note: its after picture is taken again, with
    /// the steps and page the agent now gives.
    async fn retake(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        report: &NoteReport,
        reply_text: Option<String>,
    ) -> Result<Reply, ArchonError> {
        if let Some(text) = reply_text.clone() {
            ctx.store.lock().await.design_note_change(
                &note.item,
                &note.id,
                NoteChange::Thread(thread(ReplyAuthor::Agent, text)),
            )?;
        }
        tracing::info!(note = %note.id, steps = report.steps.len(), "design: the agent retakes the after picture");
        let wt = self.worktree_of(ctx, session).await?;
        let note = ctx
            .store
            .lock()
            .await
            .design_note(&note.item, &note.id)?
            .unwrap_or_else(|| note.clone());
        self.picture_after_done(ctx, session, &note, &wt, report, reply_text)
            .await
    }

    /// What a finished capture means for the note and for the agent: nothing
    /// to say when a picture was kept; a request to retake when it matched
    /// the before picture; a notice for the operator when the retake matched
    /// too.
    async fn settle_outcome(
        &self,
        ctx: &DaemonContext,
        note: &DesignNote,
        outcome: AfterOutcome,
        reply_text: Option<String>,
    ) -> Result<Reply, ArchonError> {
        let id = note.id.as_str();
        match outcome {
            AfterOutcome::Kept | AfterOutcome::Skipped => {
                self.note_reply(ctx, &note.item, id).await
            }
            AfterOutcome::StepFailed(why) => Ok(Reply::DesignAgent {
                message: format!(
                    "{id}: the after picture was not taken: {why}. Fix the steps and report done \
                     again for {id} with the same commit."
                ),
            }),
            AfterOutcome::Matched if note.after_asset.is_some() => Ok(Reply::DesignAgent {
                message: format!(
                    "{id}: the new picture matches the before picture, so the earlier after \
                     picture stays."
                ),
            }),
            AfterOutcome::Matched => {
                let said = reply_text.or_else(|| last_agent_words(note));
                let message = self.matched_before(ctx, note, said).await?;
                Ok(Reply::DesignAgent { message })
            }
        }
    }

    /// The after picture matched the before one: ask for a retake the first
    /// time, show the operator a notice after that. Returns what the agent is
    /// told.
    async fn matched_before(
        &self,
        ctx: &DaemonContext,
        note: &DesignNote,
        said: Option<String>,
    ) -> Result<String, ArchonError> {
        let id = note.id.as_str();
        let (text, message) = if archon_core::design::retake_asked(note) {
            let why = said.unwrap_or_else(|| "the agent gave no explanation".into());
            (
                format!("{NO_VISIBLE_CHANGE}: {why}"),
                format!(
                    "{id}: the after picture still matches the before picture. The operator sees \
                     \"no visible change captured\" with your reply as the explanation."
                ),
            )
        } else {
            (
                format!(
                    "{RETAKE_ASKED}, so it shows no change; the agent was asked to retake it in \
                     the state that shows the change."
                ),
                format!(
                    "{id}: the after picture is identical to the before picture, so it shows no \
                     change. Report done again for {id} with the same commit and `steps` that \
                     open the state showing the change (click or hover what opens the dialog or \
                     menu; `path` for a page), and say in `reply` what changed. If nothing \
                     visible changed, say why in `reply` and report done again."
                ),
            )
        };
        ctx.store.lock().await.design_note_change(
            &note.item,
            id,
            NoteChange::Thread(thread(ReplyAuthor::Archon, text)),
        )?;
        self.emit_note(ctx, &note.item, id).await;
        Ok(message)
    }

    pub(crate) async fn keep_after(
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
        steps: Box::default(),
    }
}

/// What the agent reports about a note.
#[derive(Debug, Clone)]
pub(crate) struct NoteReport {
    pub id: String,
    pub status: AgentNoteStatus,
    pub reply: Option<String>,
    pub commit: Option<String>,
    pub no_change: bool,
    pub path: Option<String>,
    pub steps: Vec<UiStep>,
}

/// The arguments that make no sense, refused before any note is read.
fn check_report(report: &NoteReport) -> Result<(), ArchonError> {
    let done_with_change = report.status == AgentNoteStatus::Done && !report.no_change;
    if let Some(path) = &report.path {
        archon_core::design::check_app_path(path)
            .map_err(|why| ArchonError::Invalid(format!("path {why}")))?;
        if !done_with_change {
            return Err(ArchonError::Invalid(
                "path goes with done and a change: the page that shows it".into(),
            ));
        }
    }
    archon_core::design::check_steps(&report.steps)
        .map_err(|why| ArchonError::Invalid(format!("steps: {why}")))?;
    if !report.steps.is_empty() && !done_with_change && report.status != AgentNoteStatus::Working {
        return Err(ArchonError::Invalid(
            "steps go with working (the before picture) or done and a change (the after picture)"
                .into(),
        ));
    }
    Ok(())
}

/// How a report stands against its note.
#[derive(Debug, PartialEq, Eq)]
enum Standing {
    /// The note is with the agent.
    InFlight,
    /// The note is done and the agent takes its after picture again.
    Retake,
}

/// Whether `report` may be made on `note` by the agent of `session`; the
/// refusal says why not in words the agent can act on.
fn standing(
    note: &DesignNote,
    session: &DesignSession,
    all: &[DesignNote],
    report: &NoteReport,
) -> Result<Standing, ArchonError> {
    let id = &note.id;
    let no = |why: String| Err(refuse(DesignRefusal::NotYours, why));
    if note.session != session.id {
        return no(format!(
            "{id} belongs to an earlier design session, which is over; its notes cannot be reported on"
        ));
    }
    if note.kind == NoteKind::Revert {
        return no(format!(
            "{id} is a revert note: Archon applies it itself, there is nothing to report"
        ));
    }
    match note.status {
        status if is_in_flight(status) => Ok(Standing::InFlight),
        NoteStatus::Queued => no(format!(
            "{id} has not been sent to you yet; report it once its message arrives"
        )),
        NoteStatus::Done => retake_standing(note, session, all, report),
        status => no(format!(
            "{id} is {} and closed; the operator can send it again as a new note",
            format!("{status:?}").to_lowercase()
        )),
    }
}

fn retake_standing(
    note: &DesignNote,
    session: &DesignSession,
    all: &[DesignNote],
    report: &NoteReport,
) -> Result<Standing, ArchonError> {
    let id = &note.id;
    let no = |why: String| Err(refuse(DesignRefusal::NotYours, why));
    if report.status != AgentNoteStatus::Done || report.no_change {
        return no(format!(
            "{id} is already done; only a repeated `done` for the same commit retakes its after picture"
        ));
    }
    if note.no_change {
        return no(format!(
            "{id} was reported with no change, so there is no after picture to retake"
        ));
    }
    if let Some(by) = archon_core::design::undone_by(all).get(id) {
        return no(format!(
            "{id} was undone by {by}; there is nothing to retake"
        ));
    }
    if let Some(by) = &note.superseded_by {
        return no(format!(
            "{id} was superseded by {by}; there is nothing to retake"
        ));
    }
    if report.commit.is_some() && report.commit != note.commit {
        return no(format!(
            "{id} is done at commit {}; a retake pictures that commit. A different change needs a new note from the operator",
            note.commit.as_deref().unwrap_or("unknown")
        ));
    }
    if !matches!(session.state, SessionState::Starting | SessionState::Live) {
        return Err(refuse(
            DesignRefusal::Finishing,
            format!("the operator is finishing the session; {id}'s pictures are frozen"),
        ));
    }
    Ok(Standing::Retake)
}

fn last_agent_words(note: &DesignNote) -> Option<String> {
    note.thread
        .iter()
        .rev()
        .find(|e| e.from == ReplyAuthor::Agent)
        .map(|e| e.text.clone())
}

fn thread(from: ReplyAuthor, text: String) -> ThreadEntry {
    ThreadEntry {
        at: archon_core::now(),
        from,
        text,
    }
}
