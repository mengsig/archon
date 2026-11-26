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
use archon_queue::store::{NoteChange, Store};

use super::assets::NewAsset;
use super::state::{AfterOutcome, AfterShot};
use super::{DesignService, WorktreeRef, refuse};
use crate::context::DaemonContext;
use crate::worktree::design_git::{self, NoteGit, Shape, Tidy};

fn preview_authority(url: &str) -> Option<&str> {
    url.strip_prefix("http://")?.split(['/', '?', '#']).next()
}

fn denied() -> ArchonError {
    ArchonError::Forbidden("only the agent of a running design stage may use design tools".into())
}

fn agent_stage(
    store: &Store,
    caller: &AetherAgentId,
) -> Result<archon_core::run::StageRun, ArchonError> {
    let stage = store.stage_run_by_agent(caller)?.ok_or_else(denied)?;
    if stage.state != archon_core::run::StageState::Running
        || stage.kind != Some(archon_core::catalog::StageKind::Design)
    {
        return Err(denied());
    }
    let run = store.run(&stage.run)?;
    if run.state.is_terminal() || store.item(&run.item)?.state.is_terminal() {
        return Err(denied());
    }
    Ok(stage)
}

fn matches_owner(
    session: &DesignSession,
    stage: &archon_core::run::StageRun,
    caller: &AetherAgentId,
) -> bool {
    session.run == stage.run
        && session.stage_id == stage.stage_id
        && session.attempt == stage.attempt
        && session.agent.as_ref() == Some(caller)
}

async fn bound_agent_session(
    ctx: &DaemonContext,
    expected: &DesignSession,
    caller: Option<&AetherAgentId>,
) -> Result<DesignSession, ArchonError> {
    let caller = caller.ok_or_else(denied)?;
    let store = ctx.store.lock().await;
    let stage = agent_stage(&store, caller)?;
    let session = store
        .design_current_session(&expected.item)?
        .filter(|s| s.id == expected.id)
        .ok_or_else(super::gone)?;
    if !matches_owner(&session, &stage, caller) {
        return Err(denied());
    }
    Ok(session)
}

impl DesignService {
    /// The session whose agent `caller` is, while its stage attempt is live.
    pub(crate) async fn agent_session(
        &self,
        ctx: &DaemonContext,
        caller: Option<&AetherAgentId>,
    ) -> Result<DesignSession, ArchonError> {
        let caller = caller.ok_or_else(|| {
            ArchonError::Forbidden("design tools are for the stage's agent".into())
        })?;
        let (stage, run, repo) = {
            let store = ctx.store.lock().await;
            let stage = agent_stage(&store, caller)?;
            let run = store.run(&stage.run)?;
            let item = store.item(&run.item)?;
            if let Some(session) = store.design_current_session(&item.id)?
                && matches_owner(&session, &stage, caller)
            {
                return Ok(session);
            }
            let repo = store.repo(&crate::resolve::repo_ref(
                item.repo.as_ref().ok_or_else(denied)?,
            ))?;
            (stage, run, repo.path)
        };
        let wt = WorktreeRef {
            repo,
            worktree: run.workspace,
            branch: run.branch,
        };
        let id = self
            .stage_started(
                &ctx.arc(),
                &run.item,
                &run.id,
                &stage.stage_id,
                stage.attempt,
                caller,
                wt,
            )
            .await?;
        let session = self.current(ctx, &id).await?;
        bound_agent_session(ctx, &session, Some(caller)).await
    }

    /// Point the preview at `url` once its server is the worktree's and the
    /// stack answers. `recipe` is saved first, so a failed save exposes nothing.
    pub(crate) async fn design_preview(
        &self,
        ctx: &DaemonContext,
        url: &str,
        recipe: Option<archon_core::design::recipe::PreviewRecipe>,
        caller: Option<&AetherAgentId>,
    ) -> Result<Reply, ArchonError> {
        let url = validate_preview_url(url)?;
        if let Some(recipe) = &recipe {
            recipe.check().map_err(ArchonError::Invalid)?;
        }
        let session = self.agent_session(ctx, caller).await?;
        let wt = self.worktree_of(ctx, &session).await?;
        self.factory().verify_server(&wt.worktree, &url).await?;
        self.factory().check_stack(&url, recipe.as_ref()).await?;
        let lock = self.item_lock(&session.item);
        let held = lock.lock().await;
        let current = bound_agent_session(ctx, &session, caller).await?;
        if !matches!(current.state, SessionState::Starting | SessionState::Live) {
            return Err(super::finishing());
        }
        if let Some(recipe) = recipe {
            let repo = super::recipe::repo_of(ctx, &session.item).await?;
            let mut store = ctx.store.lock().await;
            super::recipe::save(&mut store, &repo, &session.item, recipe)?;
            tracing::info!(%repo, item = %session.item, "design: the preview recipe was saved");
        }
        let request = self.register_preview(ctx, &current, &url).await?;
        self.update_preview_browser(&current, &url).await?;
        drop(held);
        self.emit_session(ctx, &session.id).await;
        if let Some(note) = request {
            self.emit_note(ctx, &session.item, &note.id).await;
            self.pump(ctx, &session.item).await?;
        }
        Ok(Reply::DesignDone)
    }

    async fn register_preview(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        url: &str,
    ) -> Result<Option<DesignNote>, ArchonError> {
        let (title, note) = {
            let mut store = ctx.store.lock().await;
            store.design_session_patch(
                &session.id,
                archon_queue::store::SessionPatch {
                    preview_url: Some(Some(url.to_owned())),
                    degraded: Some(false),
                    ..Default::default()
                },
            )?;
            let title = if session.state == SessionState::Starting {
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
            (title, super::revise_note(&mut store, session)?)
        };
        if let Some(title) = title {
            let title = crate::executor::clip(title.trim(), 80);
            crate::feed::notify(
                ctx,
                archon_core::notify::NotificationKind::Blocker,
                format!("Design session ready: {title}. Open it to show the agent what to change."),
                Some(session.item.clone()),
                Some(session.run.clone()),
            )
            .await?;
        }
        Ok(note)
    }

    async fn update_preview_browser(
        &self,
        row: &DesignSession,
        url: &str,
    ) -> Result<(), ArchonError> {
        let rt = self.runtime(&row.id);
        rt.st().last_url = Some(url.to_owned());
        if let Some(browser) = rt.browser() {
            if row.preview_url.as_deref().and_then(preview_authority) == preview_authority(url)
                && browser.probe(url).await
            {
                browser.navigate(url).await.map_err(|e| {
                    ArchonError::Unavailable(format!("navigating the preview: {e}"))
                })?;
            } else {
                self.stop_browser(&row.id, "dev server changed", false)
                    .await;
            }
        }
        let watched = rt.st().subscribers > 0;
        if watched && rt.browser().is_none() {
            self.spawn_first_look(&row.id);
        }
        Ok(())
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
        let current = bound_agent_session(ctx, &session, caller).await?;
        let result = self.report_locked(ctx, &current, report).await;
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
            let entry = reply_text.map(|text| thread(ReplyAuthor::Agent, text));
            match (note.status, entry) {
                (NoteStatus::Working, Some(entry)) => {
                    store.design_note_change(item, id, NoteChange::Thread(entry))?;
                }
                (NoteStatus::Working, None) => {}
                (_, Some(entry)) => {
                    store.design_note_transition(
                        item,
                        id,
                        super::status_change(NoteStatus::Working, None),
                        Some(entry),
                        None,
                    )?;
                }
                (_, None) => {
                    store.design_note_change(
                        item,
                        id,
                        super::status_change(NoteStatus::Working, None),
                    )?;
                }
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
        let to = self
            .record_settled(ctx, item, &report, reply_text.clone(), done_commit)
            .await?;
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

    /// Write an agent's settling report: its reply, then the new status.
    /// Returns the status the note is now in.
    async fn record_settled(
        &self,
        ctx: &DaemonContext,
        item: &archon_core::id::QueueItemId,
        report: &NoteReport,
        reply_text: Option<String>,
        done_commit: Option<String>,
    ) -> Result<NoteStatus, ArchonError> {
        let (id, status) = (report.id.as_str(), report.status);
        let (to, fail) = match status {
            AgentNoteStatus::Done => (NoteStatus::Done, None),
            AgentNoteStatus::NeedsReply => (NoteStatus::NeedsReply, None),
            AgentNoteStatus::Failed => (NoteStatus::Failed, Some(FailReason::Agent)),
            AgentNoteStatus::Cancelled => (NoteStatus::Cancelled, None),
            AgentNoteStatus::Working => unreachable!("handled by the caller"),
        };
        let mut store = ctx.store.lock().await;
        let change = NoteChange::Status {
            to,
            fail_reason: fail,
            commit: done_commit,
            no_change: report.no_change && status == AgentNoteStatus::Done,
        };
        if let Some(text) = reply_text {
            store.design_note_transition(
                item,
                id,
                change,
                Some(thread(ReplyAuthor::Agent, text)),
                None,
            )?;
        } else {
            store.design_note_change(item, id, change)?;
        }
        Ok(to)
    }

    async fn history_changed(
        &self,
        ctx: &DaemonContext,
        item: &archon_core::id::QueueItemId,
        id: &str,
        why: String,
    ) -> Result<Reply, ArchonError> {
        tracing::info!(note = id, why, "design: history changed");
        let text = "The code history changed in a way Archon can't track; its edits may still show; add a note if something should change.".to_owned();
        ctx.store.lock().await.design_note_transition(
            item,
            id,
            super::status_change(NoteStatus::Failed, Some(FailReason::HistoryChanged)),
            Some(thread(ReplyAuthor::Archon, text)),
            None,
        )?;
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
                    capture: Some(super::assets::capture_of(picture)),
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
    if !archon_core::design::valid_note_id(&report.id) {
        return Err(ArchonError::Invalid(
            "id must be a note ID such as D3".into(),
        ));
    }
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
