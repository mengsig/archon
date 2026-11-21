//! Live Design Sessions (design.md): archond's side of the server-side
//! preview. One session per `design` stage attempt: a sandboxed headless
//! browser the operator sees as pictures, a FIFO note queue to the stage's
//! kept agent, archond's own commit checks and undo, and the finish capture
//! the proposal gate's ledger is built from.

mod assets;
mod browser;
mod delivery;
mod finish;
mod notes;
mod recovery;
mod relay;
mod repo_hosts;
mod reports;
mod requests;
mod undo;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

use archon_core::aether_ids::AetherAgentId;
use archon_core::config::DesignConfig;
use archon_core::design::{
    DesignAsset, DesignNote, DesignSession, DesignSessionId, FinishChoices, NoteKind, NoteStatus,
    SessionState, auto_supersession, undone_by,
};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::id::PipelineRunId as RunId;
use archon_core::id::QueueItemId;
use archon_core::protocol::{DesignEvent, DesignNoteView, WelcomeDevice};
use archon_queue::Store;

pub use self::browser::{BrowserFactory, FrameFeed, PreviewBrowser, RealFactory};
pub(crate) use self::delivery::sweep;
pub use self::recovery::recover;
pub use self::relay::stream;
pub(crate) use self::requests::handle;

use crate::context::DaemonContext;
use crate::worktree::design_git::{self, NoteGit};
use crate::worktree::shadow::Shadow;

/// One stage attempt's worktree: the repository, the worktree its agent
/// edits and the branch checked out in it.
#[derive(Debug, Clone)]
pub struct WorktreeRef {
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
}

/// How a design stage ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageEnd {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl StageEnd {
    fn reason(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed out",
        }
    }
}

tokio::task_local! {
    static DEVICE: WelcomeDevice;
}

/// Run `work` as the paired device `device`: design requests it makes are
/// audited and attributed to it.
pub(crate) async fn with_device<F: Future>(device: WelcomeDevice, work: F) -> F::Output {
    DEVICE.scope(device, work).await
}

pub(crate) fn current_device() -> Option<WelcomeDevice> {
    DEVICE.try_with(Clone::clone).ok()
}

/// "id (role)" of the device making this request, for audit lines.
pub(crate) fn current_device_label() -> Option<String> {
    current_device().map(|d| format!("{} ({:?})", d.id, d.role))
}

pub(crate) fn refuse(code: DesignRefusal, message: impl Into<String>) -> ArchonError {
    ArchonError::Design {
        code,
        message: message.into(),
    }
}

pub(crate) fn gone() -> ArchonError {
    refuse(DesignRefusal::Gone, "this design session has ended")
}

pub(crate) fn finishing() -> ArchonError {
    refuse(
        DesignRefusal::Finishing,
        "Finishing: press Back to editing to change anything",
    )
}

/// The design service: sessions' browsers, the note loop and the finish.
pub struct DesignService {
    pub(crate) cfg: DesignConfig,
    pub(crate) data_dir: PathBuf,
    weak: OnceLock<Weak<DaemonContext>>,
    factory: std::sync::Mutex<Arc<dyn BrowserFactory>>,
    pub(crate) rt: std::sync::Mutex<HashMap<DesignSessionId, Arc<browser::Runtime>>>,
    wts: std::sync::Mutex<HashMap<DesignSessionId, WorktreeRef>>,
    item_locks: std::sync::Mutex<HashMap<QueueItemId, Arc<tokio::sync::Mutex<()>>>>,
    pub(crate) owed_lock: tokio::sync::Mutex<()>,
    /// Streams' flow control: (device, subscription) to the last frame acked.
    pub(crate) acks: std::sync::Mutex<HashMap<(String, u64), Arc<relay::AckState>>>,
}

impl std::fmt::Debug for DesignService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesignService")
            .field("enabled", &self.cfg.enabled)
            .finish_non_exhaustive()
    }
}

impl DesignService {
    pub fn new(cfg: &DesignConfig, data_dir: PathBuf) -> Self {
        Self {
            cfg: cfg.clone(),
            data_dir,
            weak: OnceLock::new(),
            factory: std::sync::Mutex::new(Arc::new(RealFactory)),
            rt: Default::default(),
            wts: Default::default(),
            item_locks: Default::default(),
            owed_lock: Default::default(),
            acks: Default::default(),
        }
    }

    /// Let background tasks reach the context that owns this service.
    pub fn bind(&self, ctx: &Arc<DaemonContext>) {
        let _ = self.weak.set(Arc::downgrade(ctx));
    }

    pub(crate) fn arc(&self) -> Option<Arc<DaemonContext>> {
        self.weak.get().and_then(Weak::upgrade)
    }

    /// Replace how browsers start (tests).
    pub fn set_factory(&self, factory: Arc<dyn BrowserFactory>) {
        *self.factory.lock().unwrap_or_else(|e| e.into_inner()) = factory;
    }

    pub(crate) fn factory(&self) -> Arc<dyn BrowserFactory> {
        self.factory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn item_lock_for_tests(&self, item: &QueueItemId) -> Arc<tokio::sync::Mutex<()>> {
        self.item_lock(item)
    }

    pub(crate) fn item_lock(&self, item: &QueueItemId) -> Arc<tokio::sync::Mutex<()>> {
        self.item_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(item.clone())
            .or_default()
            .clone()
    }

    // ---- stage lifecycle -------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub async fn stage_started(
        &self,
        ctx: &Arc<DaemonContext>,
        item: &QueueItemId,
        run: &RunId,
        stage_id: &str,
        attempt: u32,
        agent: &AetherAgentId,
        worktree: WorktreeRef,
    ) -> Result<DesignSessionId, ArchonError> {
        if !self.enabled() {
            return Err(ArchonError::Invalid(
                "design sessions are turned off ([design] enabled = false)".into(),
            ));
        }
        self.bind(ctx);
        let mut store = ctx.store.lock().await;
        if let Some(found) = self.session_of(&store, run, stage_id, attempt)?
            && found.state != SessionState::Ended
        {
            // A fresh agent replaced the attempt's (a refused or lost agent
            // at the same attempt): the session, its tools and its owed
            // notes follow the agent now running.
            if found.agent.as_ref() != Some(agent) {
                store.design_session_patch(
                    &found.id,
                    archon_queue::store::SessionPatch {
                        agent: Some(agent.clone()),
                        ..Default::default()
                    },
                )?;
                tracing::info!(session = %found.id, %agent, "design: session follows the attempt's new agent");
            }
            self.wts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(found.id.clone(), worktree);
            return Ok(found.id);
        }
        let session = store.design_session_create(archon_queue::store::NewDesignSession {
            item: item.clone(),
            run: run.clone(),
            stage_id: stage_id.to_owned(),
            attempt,
            viewport_preset: archon_core::design::ViewportPreset::Desktop,
            sandbox_off: self.cfg.browser_sandbox == archon_core::config::BrowserSandbox::Off,
            agent: Some(agent.clone()),
        })?;
        drop(store);
        self.wts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.id.clone(), worktree);
        tracing::info!(session = %session.id, %item, attempt, "design: session started");
        Ok(session.id)
    }

    pub async fn stage_settled(
        &self,
        ctx: &Arc<DaemonContext>,
        run: &RunId,
        stage_id: &str,
        attempt: u32,
        outcome: StageEnd,
    ) -> Result<(), ArchonError> {
        let session = {
            let store = ctx.store.lock().await;
            self.session_of(&store, run, stage_id, attempt)?
        };
        let Some(session) = session else {
            return Ok(());
        };
        self.end_session(ctx, &session, outcome.reason()).await
    }

    pub(crate) async fn end_session(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        reason: &str,
    ) -> Result<(), ArchonError> {
        if session.state == SessionState::Ended {
            return Ok(());
        }
        let lock = self.item_lock(&session.item);
        // Callers hold the run table (a stage settling, a cancel), and a
        // note, an undo or a Finish holds the item lock while it works: never
        // wait for it here. When it is busy, the end runs on a task of its own
        // as soon as the holder lets go.
        let held = match lock.clone().try_lock_owned() {
            Ok(held) => held,
            Err(_) => match self.arc() {
                Some(arc) => {
                    let session = session.clone();
                    let reason = reason.to_owned();
                    tracing::info!(session = %session.id, "design: the item is busy; the session ends when it is free");
                    tokio::spawn(async move {
                        let held = lock.lock_owned().await;
                        if let Err(error) =
                            arc.design.end_locked(&arc, &session, &reason, held).await
                        {
                            tracing::warn!(%error, session = %session.id, "design: the session did not end");
                        }
                    });
                    return Ok(());
                }
                None => lock.lock_owned().await,
            },
        };
        self.end_locked(ctx, session, reason, held).await
    }

    /// End `session` under its item lock, `_held`.
    async fn end_locked(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        reason: &str,
        _held: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), ArchonError> {
        {
            let mut store = ctx.store.lock().await;
            // Re-read under the lock: another end may have won the race. A
            // row ended in the store alone (a newer session superseded it)
            // still lets go of what this daemon holds for it.
            match store.design_session(&session.id)? {
                Some(row) if row.state != SessionState::Ended => {}
                _ => {
                    drop(store);
                    if let Some(rt) = self.existing_runtime(&session.id) {
                        let _ = rt.events.send(DesignEvent::Ended {
                            reason: reason.to_owned(),
                        });
                        self.close_session_browser(&session.id, reason).await;
                        self.spawn_start_queued();
                    }
                    self.wts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&session.id);
                    return Ok(());
                }
            }
            // Notes the agent never settled stay on record as not delivered.
            for note in store.design_notes(&session.item)? {
                if note.session != session.id {
                    continue;
                }
                match note.status {
                    NoteStatus::Queued => {
                        store.design_note_change(
                            &session.item,
                            &note.id,
                            status_change(
                                NoteStatus::Cancelled,
                                Some(archon_core::design::FailReason::NotDelivered),
                            ),
                        )?;
                    }
                    NoteStatus::Sent | NoteStatus::Working | NoteStatus::NeedsReply
                        if note.kind != NoteKind::Revert =>
                    {
                        store.design_note_change(
                            &session.item,
                            &note.id,
                            status_change(
                                NoteStatus::Failed,
                                Some(archon_core::design::FailReason::NotDelivered),
                            ),
                        )?;
                    }
                    _ => {}
                }
            }
            store.design_session_end(&session.id, reason)?;
            store.kv_delete(&archon_queue::store::keys::design_owed(&session.id))?;
        }
        if let Some(rt) = self.existing_runtime(&session.id) {
            let _ = rt.events.send(DesignEvent::Ended {
                reason: reason.to_owned(),
            });
        }
        self.close_session_browser(&session.id, reason).await;
        // A slot is free: start a queued session's browser, off this path,
        // which may hold the run table.
        if let Some(arc) = self.arc() {
            tokio::spawn(async move { arc.design.start_queued(&arc).await });
        }
        self.wts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session.id);
        tracing::info!(session = %session.id, reason, "design: session ended");
        Ok(())
    }

    /// Stop a confirmed session's browser and delete its profile while the
    /// session itself stays on record for its gate: nothing is previewed
    /// after Confirm. Its runtime stays, so open streams still get the
    /// session's later events and its end. Off the caller's path, which may
    /// hold the run table.
    pub(crate) fn park_browser(&self, session: &DesignSessionId, reason: &'static str) {
        let Some(arc) = self.arc() else {
            return;
        };
        let session = session.clone();
        tokio::spawn(async move {
            arc.design.stop_browser(&session, reason, false).await;
            arc.design.start_queued(&arc).await;
        });
    }

    pub async fn gate_decided(
        &self,
        _ctx: &Arc<DaemonContext>,
        run: &RunId,
        stage_id: &str,
        attempt: u32,
        decision: &str,
    ) -> Result<(), ArchonError> {
        tracing::info!(%run, stage_id, attempt, decision, device = ?current_device_label(), "design: gate answered");
        Ok(())
    }

    pub fn session_of(
        &self,
        store: &Store,
        run: &RunId,
        stage_id: &str,
        attempt: u32,
    ) -> Result<Option<DesignSession>, ArchonError> {
        let all = store.design_sessions_in(&[
            SessionState::Starting,
            SessionState::Live,
            SessionState::Finishing,
            SessionState::Confirmed,
            SessionState::Ended,
        ])?;
        // A retried slot reuses its attempt number: the open session wins,
        // else the newest (the store lists oldest first).
        let mine = all
            .into_iter()
            .filter(|s| &s.run == run && s.stage_id == stage_id && s.attempt == attempt)
            .collect::<Vec<_>>();
        let open = mine.iter().rposition(|s| s.state != SessionState::Ended);
        Ok(match open {
            Some(at) => mine.into_iter().nth(at),
            None => mine.into_iter().last(),
        })
    }

    // ---- worktree ----------------------------------------------------------

    /// The worktree a session's stage runs in: remembered, else read from
    /// the run's record (after a restart).
    pub(crate) async fn worktree_of(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
    ) -> Result<WorktreeRef, ArchonError> {
        if let Some(found) = self
            .wts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session.id)
        {
            return Ok(found.clone());
        }
        let store = ctx.store.lock().await;
        let run = store.run(&session.run)?;
        let item = store.item(&run.item)?;
        let repo_id = item
            .repo
            .ok_or_else(|| ArchonError::Invalid("a design item has a repository".into()))?;
        let repo = store
            .repos()?
            .into_iter()
            .find(|r| r.id == repo_id)
            .ok_or_else(|| ArchonError::Invalid(format!("repository {repo_id} is gone")))?;
        drop(store);
        let wt = WorktreeRef {
            repo: repo.path,
            worktree: run.workspace,
            branch: run.branch,
        };
        self.wts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.id.clone(), wt.clone());
        Ok(wt)
    }

    pub(crate) fn shadow(&self, wt: &WorktreeRef) -> Result<Shadow, ArchonError> {
        Shadow::of(&self.data_dir, &wt.repo)
    }

    pub(crate) fn head_of(&self, wt: &WorktreeRef) -> Result<String, ArchonError> {
        let shadow = self.shadow(wt)?;
        design_git::head(&NoteGit {
            shadow: &shadow,
            repo: &wt.repo,
            worktree: &wt.worktree,
            branch: &wt.branch,
        })
    }

    /// `Err(plain text)` unless HEAD is `head` and the tree is tidy.
    pub fn worktree_at(
        &self,
        wt: &WorktreeRef,
        head: &str,
    ) -> Result<Result<(), String>, ArchonError> {
        let shadow = self.shadow(wt)?;
        let g = NoteGit {
            shadow: &shadow,
            repo: &wt.repo,
            worktree: &wt.worktree,
            branch: &wt.branch,
        };
        let now = design_git::head(&g)?;
        if now != head {
            return Ok(Err(format!(
                "worktree moved after Finish: expected {}; restore it and complete again",
                short(head)
            )));
        }
        match design_git::check_tidy(&g, None)? {
            design_git::Tidy::Tidy => Ok(Ok(())),
            design_git::Tidy::Untidy(why) => Ok(Err(why)),
        }
    }

    /// The notes of `item` that are active at commit `at`: done change and
    /// page notes not undone, still in the history, not already requirements
    /// of the item's approved intent, and not superseded (automatically, or
    /// by the operator's `choices`).
    pub fn active_notes(
        &self,
        store: &Store,
        wt: &WorktreeRef,
        item: &QueueItemId,
        at: &str,
        choices: Option<&FinishChoices>,
    ) -> Result<Vec<DesignNote>, ArchonError> {
        let notes = store.design_notes(item)?;
        let undone = undone_by(&notes);
        // Notes an earlier approval made requirements stay bound through the
        // approved intent the ledger starts from: never added again, and
        // never superseded by a later note (Q3).
        let approved = store
            .item(item)?
            .intent
            .map(|binding| binding.requirements)
            .unwrap_or_default();
        let shadow = self.shadow(wt)?;
        let g = NoteGit {
            shadow: &shadow,
            repo: &wt.repo,
            worktree: &wt.worktree,
            branch: &wt.branch,
        };
        let mut candidates = Vec::new();
        for note in &notes {
            if note.kind == NoteKind::Revert
                || note.status != NoteStatus::Done
                || undone.contains_key(&note.id)
                || approved.contains(&note.id)
            {
                continue;
            }
            let Some(commit) = &note.commit else {
                continue;
            };
            if !design_git::contains(&g, commit, at)? {
                continue;
            }
            candidates.push(note);
        }
        let auto = auto_supersession(&candidates);
        Ok(candidates
            .into_iter()
            .filter(|note| {
                let by = match choices.and_then(|c| c.superseded.get(&note.id)) {
                    Some(chosen) => chosen.clone(),
                    None => auto.get(&note.id).cloned(),
                };
                by.is_none()
            })
            .cloned()
            .collect())
    }

    pub fn asset_path(&self, asset: &DesignAsset) -> PathBuf {
        let ext = if asset.mime == "image/png" {
            "png"
        } else {
            "jpg"
        };
        self.data_dir
            .join("design")
            .join("assets")
            .join(asset.item.to_string())
            .join(format!("{}.{ext}", asset.sha256))
    }

    /// Forget an item's design data; its asset files go with it.
    pub fn gc_item(&self, store: &mut Store, item: &QueueItemId) -> Result<(), ArchonError> {
        let assets = store.design_item_delete(item)?;
        for asset in assets {
            let _ = std::fs::remove_file(self.asset_path(&asset));
        }
        let _ = std::fs::remove_dir_all(
            self.data_dir
                .join("design")
                .join("assets")
                .join(item.to_string()),
        );
        Ok(())
    }

    // ---- shared helpers ----------------------------------------------------

    /// The session a request names: the item's current one, not ended.
    pub(crate) async fn current(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
    ) -> Result<DesignSession, ArchonError> {
        let store = ctx.store.lock().await;
        let Some(row) = store.design_session(session)? else {
            return Err(gone());
        };
        if row.state == SessionState::Ended {
            return Err(gone());
        }
        match store.design_current_session(&row.item)? {
            Some(current) if current.id == row.id => Ok(row),
            _ => Err(gone()),
        }
    }

    /// The item's current session for a note request, refused while it
    /// finishes.
    pub(crate) async fn current_for_notes(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
    ) -> Result<DesignSession, ArchonError> {
        let store = ctx.store.lock().await;
        let Some(row) = store.design_current_session(item)? else {
            return Err(gone());
        };
        match row.state {
            SessionState::Ended => Err(gone()),
            SessionState::Finishing | SessionState::Confirmed => Err(finishing()),
            _ => Ok(row),
        }
    }

    pub(crate) fn emit(&self, session: &DesignSessionId, event: DesignEvent) {
        if let Some(rt) = self.existing_runtime(session) {
            let _ = rt.events.send(event);
        }
    }

    pub(crate) async fn emit_session(&self, ctx: &DaemonContext, session: &DesignSessionId) {
        let row = ctx
            .store
            .lock()
            .await
            .design_session(session)
            .ok()
            .flatten();
        let Some(row) = row else {
            return;
        };
        let status = self
            .existing_runtime(session)
            .map(|rt| rt.st().status.clone())
            .unwrap_or(archon_core::protocol::BrowserStatus::Idle);
        let preview_path = row.preview_url.as_deref().map(path_of);
        self.emit(
            session,
            DesignEvent::Session {
                state: row.state,
                degraded: row.degraded,
                preview_path,
                commit: None,
                browser: status,
                note_version: row.note_version,
            },
        );
    }

    pub(crate) async fn emit_note(&self, ctx: &DaemonContext, item: &QueueItemId, id: &str) {
        let (note, session, all) = {
            let store = ctx.store.lock().await;
            let note = store.design_note(item, id).ok().flatten();
            let session = store.design_current_session(item).ok().flatten();
            let all = store.design_notes(item).unwrap_or_default();
            (note, session, all)
        };
        let (Some(note), Some(session)) = (note, session) else {
            return;
        };
        let view = note_view(&note, &all, Some(&session));
        self.emit(&note.session, DesignEvent::Note(view));
        self.emit_session(ctx, &session.id).await;
    }

    pub(crate) async fn mark_degraded(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        degraded: bool,
        url: Option<&str>,
    ) {
        let row = ctx
            .store
            .lock()
            .await
            .design_session(session)
            .ok()
            .flatten();
        let Some(row) = row else {
            return;
        };
        if row.degraded == degraded || row.state == SessionState::Ended {
            return;
        }
        {
            let mut store = ctx.store.lock().await;
            let patch = archon_queue::store::SessionPatch {
                degraded: Some(degraded),
                ..Default::default()
            };
            if store.design_session_patch(session, patch).is_err() {
                return;
            }
        }
        self.emit_session(ctx, session).await;
        if degraded && matches!(row.state, SessionState::Live | SessionState::Starting) {
            let url = url
                .map(str::to_owned)
                .or(row.preview_url.clone())
                .unwrap_or_default();
            let incident = archon_core::now().timestamp_millis();
            let key = format!("design:{session}:preview:{incident}");
            let text = format!("preview unreachable at {url}");
            if let Err(error) = self.owe(ctx, &row, key, text, None).await {
                tracing::warn!(%error, "the preview-unreachable message was not queued");
            }
        }
    }
}

pub(crate) fn status_change(
    to: NoteStatus,
    fail_reason: Option<archon_core::design::FailReason>,
) -> archon_queue::store::NoteChange {
    archon_queue::store::NoteChange::Status {
        to,
        fail_reason,
        commit: None,
        no_change: false,
    }
}

/// The first seven characters of a commit id.
pub(crate) fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// A URL's path, without query or fragment.
pub(crate) fn path_of(url: &str) -> String {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let path = rest.find('/').map_or("/", |i| &rest[i..]);
    path.split(['?', '#']).next().unwrap_or("/").to_owned()
}

/// A note as the web app sees it.
pub(crate) fn note_view(
    note: &DesignNote,
    all: &[DesignNote],
    current: Option<&DesignSession>,
) -> DesignNoteView {
    let undone = undone_by(all);
    let read_only = current.is_none_or(|c| c.id != note.session);
    let active = note.kind != NoteKind::Revert
        && note.status == NoteStatus::Done
        && !undone.contains_key(&note.id)
        && note.superseded_by.is_none();
    DesignNoteView {
        note: note.clone(),
        undone_by: undone.get(&note.id).cloned(),
        read_only,
        active,
    }
}
