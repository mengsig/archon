//! Live design sessions: the sessions, their notes and the pictures they hold.
//!
//! A note's `D<n>` is allocated in the transaction that inserts it, from a
//! per-item counter and every id the item's intent already uses, so ids never
//! repeat across sessions, attempts or approvals. Only the item's current
//! session takes writes; earlier sessions keep their rows as they were.

use std::collections::BTreeSet;

use archon_core::aether_ids::AetherAgentId;
use archon_core::design::{
    DesignAsset, DesignAssetId, DesignNote, DesignSession, DesignSessionId, FailReason,
    FinishChoices, MAX_NOTE_CHARS, MAX_NOTES_PER_SESSION, NoteKind, NoteStatus, SessionState,
    Target, ThreadEntry, Viewport, ViewportPreset, clean_text, is_in_flight, next_note_number,
    note_id, transition_allowed, validate_preview_url,
};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::id::{PipelineRunId, QueueItemId};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::Store;
use super::codec::{SqlContext, from_json, to_i64, to_json, to_u64, ts, write_tx};
use super::settings;

/// Well-known `kv` keys.
pub mod keys {
    use archon_core::design::DesignSessionId;
    use archon_core::id::QueueItemId;

    /// The next note number of an item, never decremented.
    pub fn design_seq(item: &QueueItemId) -> String {
        format!("design_seq:{item}")
    }
    /// Present while a delivery to the session's agent is owed.
    pub fn design_owed(session: &DesignSessionId) -> String {
        format!("design_owed:{session}")
    }
}

#[derive(Debug, Clone)]
pub struct NewDesignSession {
    pub item: QueueItemId,
    pub run: PipelineRunId,
    pub stage_id: String,
    pub attempt: u32,
    pub viewport_preset: ViewportPreset,
    pub sandbox_off: bool,
    pub agent: Option<AetherAgentId>,
}

#[derive(Debug, Clone)]
pub struct NewDesignNote {
    pub item: QueueItemId,
    pub session: DesignSessionId,
    pub kind: NoteKind,
    pub text: String,
    pub details_text: Option<String>,
    pub send_picture: bool,
    pub target: Option<Target>,
    pub viewport: Viewport,
    pub client_key: String,
    pub reverts: Option<String>,
    pub resent_from: Option<String>,
    pub follows: Option<String>,
    pub refs: Vec<String>,
    pub before_asset: Option<DesignAssetId>,
    pub created_by_device: Option<String>,
}

#[derive(Debug, Clone)]
pub enum NoteChange {
    Edit {
        text: String,
    },
    Sent {
        base_commit: String,
        delivery: u32,
    },
    Redelivered {
        delivery: u32,
    },
    Status {
        to: NoteStatus,
        fail_reason: Option<FailReason>,
        commit: Option<String>,
        no_change: bool,
    },
    RevertPrepared {
        base_commit: String,
        commit: String,
    },
    Thread(ThreadEntry),
    /// The before picture, retaken in the state the agent named.
    Before {
        asset: DesignAssetId,
    },
    After {
        asset: DesignAssetId,
        captured_commit: String,
    },
    Final {
        asset: Option<DesignAssetId>,
        included: Option<bool>,
        captured_commit: Option<String>,
    },
    Superseded {
        by: Option<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct SessionPatch {
    pub preview_url: Option<Option<String>>,
    pub browser: Option<Option<(u32, u64)>>,
    pub viewport_preset: Option<ViewportPreset>,
    pub degraded: Option<bool>,
    /// The stage attempt's agent, when a fresh agent replaced it.
    pub agent: Option<AetherAgentId>,
}

#[derive(Debug, Clone)]
pub struct FinishRecord {
    pub capture: String,
    pub head: String,
    pub note_version: u64,
    pub choices: Option<FinishChoices>,
}

fn refuse(code: DesignRefusal, message: impl Into<String>) -> ArchonError {
    ArchonError::Design {
        code,
        message: message.into(),
    }
}

/// An enum as its snake_case text.
fn enc<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => unreachable!("design enums serialize as strings"),
    }
}

fn dec<T: DeserializeOwned>(text: &str, what: &str) -> Result<T, ArchonError> {
    from_json(&to_json(&text, what)?, what)
}

fn dec_opt<T: DeserializeOwned>(
    text: Option<String>,
    what: &str,
) -> Result<Option<T>, ArchonError> {
    text.map(|t| dec(&t, what)).transpose()
}

fn parse_ts(text: &str) -> Result<DateTime<Utc>, ArchonError> {
    DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|e| ArchonError::Corrupt(format!("design timestamp {text:?}: {e}")))
}

fn corrupt<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> ArchonError + '_ {
    move |e| ArchonError::Corrupt(format!("design {what}: {e}"))
}

const SESSION_COLUMNS: &str = "id, item, run, stage_id, attempt, state, degraded, note_version,
    finish_capture, finish_head, finish_choices_json, preview_url, browser_pid,
    browser_started_at, viewport_preset, sandbox_off, agent, created_at, ended_at, end_reason";

fn session_row(row: &Row<'_>) -> rusqlite::Result<Result<DesignSession, ArchonError>> {
    let id: String = row.get(0)?;
    let item: String = row.get(1)?;
    let run: String = row.get(2)?;
    let state: String = row.get(5)?;
    let choices: Option<String> = row.get(10)?;
    let pid: Option<i64> = row.get(12)?;
    let started: Option<i64> = row.get(13)?;
    let preset: String = row.get(14)?;
    let agent: Option<String> = row.get(16)?;
    let created: String = row.get(17)?;
    let ended: Option<String> = row.get(18)?;
    let attempt: i64 = row.get(4)?;
    let note_version: i64 = row.get(7)?;
    let build = || -> Result<DesignSession, ArchonError> {
        Ok(DesignSession {
            id: DesignSessionId::parse(&id)?,
            item: QueueItemId::parse(&item)?,
            run: PipelineRunId::parse(&run)?,
            stage_id: row.get(3).ctx("read design session")?,
            attempt: u32::try_from(attempt).map_err(corrupt("attempt"))?,
            state: dec(&state, "session state")?,
            degraded: row.get(6).ctx("read design session")?,
            note_version: to_u64(note_version, "note version")?,
            finish_capture: row.get(8).ctx("read design session")?,
            finish_head: row.get(9).ctx("read design session")?,
            finish_choices: choices
                .map(|c| from_json(&c, "finish choices"))
                .transpose()?,
            preview_url: row.get(11).ctx("read design session")?,
            browser_pid: pid
                .map(|p| u32::try_from(p).map_err(corrupt("browser pid")))
                .transpose()?,
            browser_started_at: started.map(|s| to_u64(s, "browser start")).transpose()?,
            viewport_preset: dec(&preset, "viewport preset")?,
            sandbox_off: row.get(15).ctx("read design session")?,
            agent: agent.map(|a| AetherAgentId::parse(&a)).transpose()?,
            created_at: parse_ts(&created)?,
            ended_at: ended.map(|e| parse_ts(&e)).transpose()?,
            end_reason: row.get(19).ctx("read design session")?,
        })
    };
    Ok(build())
}

fn read_sessions(
    conn: &Connection,
    clause: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<DesignSession>, ArchonError> {
    let sql = format!("SELECT {SESSION_COLUMNS} FROM design_sessions {clause}");
    let mut statement = conn.prepare_cached(&sql).ctx("prepare design sessions")?;
    let rows = statement
        .query_map(args, session_row)
        .ctx("read design sessions")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.ctx("read design sessions")??);
    }
    Ok(out)
}

fn session_by_id(
    conn: &Connection,
    id: &DesignSessionId,
) -> Result<Option<DesignSession>, ArchonError> {
    Ok(read_sessions(conn, "WHERE id = ?1", &[&id.as_str()])?.pop())
}

/// The item's newest session that has not ended.
fn current_session(
    conn: &Connection,
    item: &QueueItemId,
) -> Result<Option<DesignSession>, ArchonError> {
    Ok(read_sessions(
        conn,
        "WHERE item = ?1 AND state <> 'ended' ORDER BY rowid DESC LIMIT 1",
        &[&item.as_str()],
    )?
    .pop())
}

const NOTE_COLUMNS: &str = "item, id, number, session, kind, text, details_text, send_picture,
    target_json, viewport_json, status, fail_reason, base_commit, commit_sha, no_change, reverts,
    resent_from, superseded_by, reply_thread_json, client_key, delivery, before_asset,
    after_asset, final_asset, final_included, captured_commit, created_by_device, created_at,
    updated_at, follows, refs_json";

fn asset_id(text: Option<String>) -> Result<Option<DesignAssetId>, ArchonError> {
    text.map(DesignAssetId::try_from).transpose()
}

fn note_row(row: &Row<'_>) -> rusqlite::Result<Result<DesignNote, ArchonError>> {
    let item: String = row.get(0)?;
    let number: i64 = row.get(2)?;
    let kind: String = row.get(4)?;
    let target: Option<String> = row.get(8)?;
    let viewport: String = row.get(9)?;
    let status: String = row.get(10)?;
    let fail: Option<String> = row.get(11)?;
    let thread: String = row.get(18)?;
    let delivery: i64 = row.get(20)?;
    let created: String = row.get(27)?;
    let updated: String = row.get(28)?;
    let refs: String = row.get(30)?;
    let build = || -> Result<DesignNote, ArchonError> {
        fn ctx<T>(r: rusqlite::Result<T>) -> Result<T, ArchonError> {
            r.ctx("read design note")
        }
        Ok(DesignNote {
            item: QueueItemId::parse(&item)?,
            id: ctx(row.get(1))?,
            number: to_u64(number, "note number")?,
            session: DesignSessionId::try_from(ctx(row.get::<_, String>(3))?)?,
            kind: dec(&kind, "note kind")?,
            text: ctx(row.get(5))?,
            details_text: ctx(row.get(6))?,
            send_picture: ctx(row.get(7))?,
            target: target.map(|t| from_json(&t, "note target")).transpose()?,
            viewport: from_json(&viewport, "note viewport")?,
            status: dec(&status, "note status")?,
            fail_reason: dec_opt(fail, "fail reason")?,
            base_commit: ctx(row.get(12))?,
            commit: ctx(row.get(13))?,
            no_change: ctx(row.get(14))?,
            reverts: ctx(row.get(15))?,
            resent_from: ctx(row.get(16))?,
            follows: ctx(row.get(29))?,
            refs: from_json(&refs, "note refs")?,
            superseded_by: ctx(row.get(17))?,
            thread: from_json(&thread, "note thread")?,
            client_key: ctx(row.get(19))?,
            delivery: u32::try_from(delivery).map_err(corrupt("delivery"))?,
            before_asset: asset_id(ctx(row.get(21))?)?,
            after_asset: asset_id(ctx(row.get(22))?)?,
            final_asset: asset_id(ctx(row.get(23))?)?,
            final_included: ctx(row.get(24))?,
            captured_commit: ctx(row.get(25))?,
            created_by_device: ctx(row.get(26))?,
            created_at: parse_ts(&created)?,
            updated_at: parse_ts(&updated)?,
        })
    };
    Ok(build())
}

fn read_notes(
    conn: &Connection,
    clause: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<DesignNote>, ArchonError> {
    let sql = format!("SELECT {NOTE_COLUMNS} FROM design_notes {clause}");
    let mut statement = conn.prepare_cached(&sql).ctx("prepare design notes")?;
    let rows = statement
        .query_map(args, note_row)
        .ctx("read design notes")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.ctx("read design notes")??);
    }
    Ok(out)
}

fn note_by_id(
    conn: &Connection,
    item: &QueueItemId,
    id: &str,
) -> Result<Option<DesignNote>, ArchonError> {
    Ok(read_notes(conn, "WHERE item = ?1 AND id = ?2", &[&item.as_str(), &id])?.pop())
}

fn session_notes(
    conn: &Connection,
    session: &DesignSessionId,
) -> Result<Vec<DesignNote>, ArchonError> {
    read_notes(
        conn,
        "WHERE session = ?1 ORDER BY number",
        &[&session.as_str()],
    )
}

/// Every requirement and image id of a record read loosely: `requirements[].id`
/// and `images[].id`.
fn record_ids(record: &serde_json::Value, ids: &mut Vec<String>) {
    for list in ["requirements", "images"] {
        if let Some(entries) = record.get(list).and_then(|v| v.as_array()) {
            ids.extend(
                entries
                    .iter()
                    .filter_map(|e| e.get("id").and_then(|i| i.as_str()))
                    .map(str::to_owned),
            );
        }
    }
}

/// The ids the item's current approved intent and its pending intent drafts use.
fn intent_ids(conn: &Connection, item: &QueueItemId) -> Result<Vec<String>, ArchonError> {
    let mut ids = Vec::new();
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM items WHERE id = ?1",
            [item.as_str()],
            |r| r.get(0),
        )
        .optional()
        .ctx("read item for design ids")?;
    if let Some(data) = data {
        let item_json: serde_json::Value = from_json(&data, "item")?;
        let binding = item_json.get("intent");
        let id = binding.and_then(|b| b.get("id")).and_then(|v| v.as_str());
        let version = binding
            .and_then(|b| b.get("version"))
            .and_then(|v| v.as_u64());
        if let (Some(id), Some(version)) = (id, version) {
            let key = format!("{}{id}:{version}", super::intent::PREFIX);
            if let Some(raw) = settings::kv_get(conn, &key)? {
                record_ids(&from_json(&raw, "approved intent")?, &mut ids);
            }
        }
    }
    let mut runs = conn
        .prepare_cached("SELECT id FROM runs WHERE item = ?1")
        .ctx("prepare runs for design ids")?;
    let runs: Vec<String> = runs
        .query_map([item.as_str()], |r| r.get(0))
        .ctx("read runs for design ids")?
        .collect::<Result<_, _>>()
        .ctx("read runs for design ids")?;
    for run in runs {
        for (_, raw) in settings::kv_prefix(conn, &format!("proposal_intent:{run}:"))? {
            let pending: serde_json::Value = from_json(&raw, "proposal intent")?;
            if let Some(record) = pending.get("record") {
                record_ids(record, &mut ids);
            }
        }
    }
    Ok(ids)
}

fn bump(tx: &Connection, session: &DesignSessionId) -> Result<(), ArchonError> {
    tx.execute(
        "UPDATE design_sessions SET note_version = note_version + 1 WHERE id = ?1",
        [&session.as_str()],
    )
    .ctx("bump note version")?;
    Ok(())
}

fn now() -> String {
    ts(&Utc::now())
}

fn end_session(tx: &Connection, id: &DesignSessionId, reason: &str) -> Result<(), ArchonError> {
    tx.execute(
        "UPDATE design_sessions SET state = 'ended', ended_at = ?2, end_reason = ?3
         WHERE id = ?1 AND state <> 'ended'",
        params![id.as_str(), now(), reason],
    )
    .ctx("end design session")?;
    Ok(())
}

fn transition_ok(from: SessionState, to: SessionState) -> bool {
    use SessionState::*;
    matches!(
        (from, to),
        (Starting, Live)
            | (Live, Finishing)
            | (Finishing, Live)
            | (Finishing, Finishing)
            | (Finishing, Confirmed)
            | (Starting | Live | Finishing | Confirmed, Ended)
    )
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "note" } else { "notes" }
}

impl Store {
    /// A new session, `Starting`; the item's previous unended session ends as superseded.
    pub fn design_session_create(
        &mut self,
        new: NewDesignSession,
    ) -> Result<DesignSession, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let id = DesignSessionId::new();
        let previous = read_sessions(
            &tx,
            "WHERE item = ?1 AND state <> 'ended'",
            &[&new.item.as_str()],
        )?;
        for old in previous {
            end_session(&tx, &old.id, "superseded")?;
        }
        tx.execute(
            "INSERT INTO design_sessions (id, item, run, stage_id, attempt, state, degraded,
                 note_version, viewport_preset, sandbox_off, agent, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'starting', 0, 0, ?6, ?7, ?8, ?9)",
            params![
                id.as_str(),
                new.item.as_str(),
                new.run.as_str(),
                new.stage_id,
                i64::from(new.attempt),
                enc(&new.viewport_preset),
                new.sandbox_off,
                new.agent.as_ref().map(|a| a.as_str().to_owned()),
                now(),
            ],
        )
        .ctx("insert design session")?;
        let session = session_by_id(&tx, &id)?.expect("the session was just inserted");
        tx.commit().ctx("commit design session")?;
        Ok(session)
    }

    pub fn design_session(
        &self,
        id: &DesignSessionId,
    ) -> Result<Option<DesignSession>, ArchonError> {
        session_by_id(&self.conn, id)
    }

    /// The item's newest session that has not ended.
    pub fn design_current_session(
        &self,
        item: &QueueItemId,
    ) -> Result<Option<DesignSession>, ArchonError> {
        current_session(&self.conn, item)
    }

    /// The open session of this attempt, or its most recently ended session.
    pub fn design_session_for_attempt(
        &self,
        run: &PipelineRunId,
        stage: &str,
        attempt: u32,
    ) -> Result<Option<DesignSession>, ArchonError> {
        Ok(read_sessions(&self.conn,
            "WHERE run = ?1 AND stage_id = ?2 AND attempt = ?3 ORDER BY (state = 'ended'), rowid DESC LIMIT 1",
            &[&run.as_str(), &stage, &attempt],
        )?.pop())
    }

    /// Every session of the item, oldest first.
    pub fn design_sessions(&self, item: &QueueItemId) -> Result<Vec<DesignSession>, ArchonError> {
        read_sessions(
            &self.conn,
            "WHERE item = ?1 ORDER BY rowid",
            &[&item.as_str()],
        )
    }

    /// Includes ended sessions, which may have been interrupted before browser cleanup.
    pub fn design_sessions_with_process(&self) -> Result<Vec<DesignSession>, ArchonError> {
        read_sessions(
            &self.conn,
            "WHERE browser_pid IS NOT NULL OR browser_started_at IS NOT NULL",
            &[],
        )
    }

    pub fn design_sessions_in(
        &self,
        states: &[SessionState],
    ) -> Result<Vec<DesignSession>, ArchonError> {
        let wanted: BTreeSet<String> = states.iter().map(enc).collect();
        let mut all = read_sessions(&self.conn, "ORDER BY rowid", &[])?;
        all.retain(|s| wanted.contains(&enc(&s.state)));
        Ok(all)
    }

    pub fn design_session_patch(
        &mut self,
        id: &DesignSessionId,
        patch: SessionPatch,
    ) -> Result<DesignSession, ArchonError> {
        let preview = match &patch.preview_url {
            Some(Some(url)) => Some(Some(validate_preview_url(url)?)),
            other => other.clone(),
        };
        let tx = write_tx(&mut self.conn)?;
        let mut session = session_by_id(&tx, id)?
            .ok_or_else(|| ArchonError::NotFound(format!("design session {id}")))?;
        if let Some(url) = preview {
            session.preview_url = url;
        }
        if let Some(browser) = patch.browser {
            session.browser_pid = browser.map(|b| b.0);
            session.browser_started_at = browser.map(|b| b.1);
        }
        if let Some(preset) = patch.viewport_preset {
            session.viewport_preset = preset;
        }
        if let Some(degraded) = patch.degraded {
            session.degraded = degraded;
        }
        if let Some(agent) = patch.agent {
            session.agent = Some(agent);
        }
        tx.execute(
            "UPDATE design_sessions SET preview_url = ?2, browser_pid = ?3,
                 browser_started_at = ?4, viewport_preset = ?5, degraded = ?6,
                 agent = ?7 WHERE id = ?1",
            params![
                id.as_str(),
                session.preview_url,
                session.browser_pid.map(i64::from),
                session
                    .browser_started_at
                    .map(|s| to_i64(s, "browser start"))
                    .transpose()?,
                enc(&session.viewport_preset),
                session.degraded,
                session.agent.as_ref().map(|a| a.as_str().to_owned()),
            ],
        )
        .ctx("patch design session")?;
        tx.commit().ctx("commit design session patch")?;
        Ok(session)
    }

    /// A compare-and-set of the session's state. Leaving `Live` for `Finishing` is refused,
    /// with the sentence the operator reads, while a note is in flight, a delivery is owed,
    /// or notes are queued and `cancel_queued` is false (true cancels them in the same
    /// transaction). `Finishing` to `Live` clears the finish fields.
    pub fn design_session_transition(
        &mut self,
        id: &DesignSessionId,
        from: SessionState,
        to: SessionState,
        finish: Option<FinishRecord>,
        cancel_queued: bool,
    ) -> Result<DesignSession, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let session = session_by_id(&tx, id)?
            .ok_or_else(|| ArchonError::NotFound(format!("design session {id}")))?;
        if session.state != from {
            return Err(refuse(
                DesignRefusal::Conflict,
                format!(
                    "the design session is {}, not {}",
                    enc(&session.state),
                    enc(&from)
                ),
            ));
        }
        if !transition_ok(from, to) {
            return Err(ArchonError::InvalidTransition(format!(
                "a design session cannot go from {} to {}",
                enc(&from),
                enc(&to)
            )));
        }
        if from == SessionState::Live && to == SessionState::Finishing {
            let notes = session_notes(&tx, id)?;
            if let Some(note) = notes.iter().find(|n| is_in_flight(n.status)) {
                return Err(ArchonError::InvalidTransition(
                    if note.status == NoteStatus::NeedsReply {
                        format!("{} needs your answer", note.id)
                    } else {
                        format!("Finish after {} is done", note.id)
                    },
                ));
            }
            if settings::kv_get(&tx, &keys::design_owed(id))?.is_some() {
                return Err(ArchonError::InvalidTransition(
                    "Finish after the message to the agent is delivered".into(),
                ));
            }
            let queued: Vec<&DesignNote> = notes
                .iter()
                .filter(|n| n.status == NoteStatus::Queued)
                .collect();
            if !queued.is_empty() {
                if !cancel_queued {
                    return Err(ArchonError::InvalidTransition(format!(
                        "Cancel {} queued {} and finish?",
                        queued.len(),
                        plural(queued.len())
                    )));
                }
                for note in &queued {
                    tx.execute(
                        "UPDATE design_notes SET status = 'cancelled', updated_at = ?3
                         WHERE item = ?1 AND id = ?2",
                        params![note.item.as_str(), note.id, now()],
                    )
                    .ctx("cancel queued design note")?;
                    bump(&tx, id)?;
                }
            }
        }
        if to == SessionState::Ended {
            end_session(&tx, id, "ended")?;
        } else {
            tx.execute(
                "UPDATE design_sessions SET state = ?2 WHERE id = ?1",
                params![id.as_str(), enc(&to)],
            )
            .ctx("transition design session")?;
        }
        if from == SessionState::Finishing && to == SessionState::Live {
            tx.execute(
                "UPDATE design_sessions SET finish_capture = NULL, finish_head = NULL,
                     finish_choices_json = NULL WHERE id = ?1",
                [&id.as_str()],
            )
            .ctx("clear design finish")?;
        }
        if let Some(record) = finish {
            let choices = record
                .choices
                .as_ref()
                .map(|c| to_json(c, "finish choices"))
                .transpose()?;
            tx.execute(
                "UPDATE design_sessions SET finish_capture = ?2, finish_head = ?3,
                     finish_choices_json = ?4 WHERE id = ?1",
                params![id.as_str(), record.capture, record.head, choices],
            )
            .ctx("record design finish")?;
        }
        let updated = session_by_id(&tx, id)?.expect("the session exists");
        tx.commit().ctx("commit design session transition")?;
        Ok(updated)
    }

    /// Commit the review, its current notes' inclusion, and the agent delivery together.
    pub fn design_finish_confirm(
        &mut self,
        id: &DesignSessionId,
        record: FinishRecord,
        inclusion: &[(String, bool)],
        owed: &str,
    ) -> Result<DesignSession, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let session = session_by_id(&tx, id)?
            .ok_or_else(|| ArchonError::NotFound(format!("design session {id}")))?;
        if session.state != SessionState::Finishing {
            return Err(refuse(
                DesignRefusal::Conflict,
                "the design session is no longer finishing",
            ));
        }
        if session.finish_capture.as_deref() != Some(&record.capture)
            || session.finish_head.as_deref() != Some(&record.head)
            || session.note_version != record.note_version
        {
            return Err(refuse(
                DesignRefusal::Stale,
                "the review changed; capture again",
            ));
        }
        let choices = record
            .choices
            .as_ref()
            .ok_or_else(|| ArchonError::Invalid("finish confirmation requires choices".into()))?;
        for (note, include) in inclusion {
            let updated = tx
                .execute(
                    "UPDATE design_notes SET final_included = ?3, updated_at = ?4
                 WHERE session = ?1 AND id = ?2 AND status = 'done'",
                    params![id.as_str(), note, include, now()],
                )
                .ctx("record final picture inclusion")?;
            if updated != 1 {
                return Err(ArchonError::Invalid(format!(
                    "{note} is not a done note of {id}"
                )));
            }
            bump(&tx, id)?;
        }
        tx.execute(
            "UPDATE design_sessions SET state = 'confirmed', finish_choices_json = ?2 WHERE id = ?1",
            params![id.as_str(), to_json(choices, "finish choices")?],
        ).ctx("confirm design session")?;
        settings::kv_set(&tx, &keys::design_owed(id), owed)?;
        let updated = session_by_id(&tx, id)?.expect("the session exists");
        assert_eq!(updated.state, SessionState::Confirmed);
        assert_eq!(updated.finish_capture, session.finish_capture);
        tx.commit().ctx("commit design finish confirmation")?;
        Ok(updated)
    }

    /// Record an applied revert and the delivery that tells its agent about the new HEAD.
    pub fn design_revert_done(
        &mut self,
        session: &DesignSession,
        id: &str,
        commit: &str,
        owed: &str,
    ) -> Result<(), ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let note = note_by_id(&tx, &session.item, id)?
            .ok_or_else(|| ArchonError::NotFound(format!("design revert {id}")))?;
        if note.session != session.id
            || note.kind != NoteKind::Revert
            || note.commit.as_deref() != Some(commit)
        {
            return Err(ArchonError::Corrupt(format!(
                "{id} does not match the prepared revert"
            )));
        }
        let done = Self::change_note(
            &tx,
            &session.item,
            id,
            NoteChange::Status {
                to: NoteStatus::Done,
                fail_reason: None,
                commit: None,
                no_change: false,
            },
        )?;
        assert_eq!(done.status, NoteStatus::Done);
        assert_eq!(done.commit, note.commit);
        settings::kv_set(&tx, &keys::design_owed(&session.id), owed)?;
        tx.commit().ctx("commit applied design revert")
    }

    /// Replace a final picture and re-stamp the review in the same write.
    pub fn design_finish_restamp(
        &mut self,
        expected: &DesignSession,
        capture: &str,
        note: &str,
        asset: DesignAssetId,
    ) -> Result<DesignSession, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let id = &expected.id;
        let session = session_by_id(&tx, id)?
            .ok_or_else(|| ArchonError::NotFound(format!("design session {id}")))?;
        if session.state != SessionState::Finishing {
            return Err(refuse(
                DesignRefusal::Conflict,
                "the design session is no longer finishing",
            ));
        }
        if session.finish_capture != expected.finish_capture
            || session.finish_head != expected.finish_head
            || session.note_version != expected.note_version
        {
            return Err(refuse(
                DesignRefusal::Stale,
                "the review changed; capture again",
            ));
        }
        Self::change_note(
            &tx,
            &session.item,
            note,
            NoteChange::Final {
                asset: Some(asset),
                included: None,
                captured_commit: session.finish_head,
            },
        )?;
        tx.execute(
            "UPDATE design_sessions SET finish_capture = ?2 WHERE id = ?1",
            params![id.as_str(), capture],
        )
        .ctx("restamp design finish")?;
        let updated = session_by_id(&tx, id)?.expect("the session exists");
        assert_eq!(updated.state, SessionState::Finishing);
        assert_eq!(updated.note_version, expected.note_version + 1);
        tx.commit().ctx("commit design finish restamp")?;
        Ok(updated)
    }

    /// End the session; `None` when it is unknown or already ended.
    pub fn design_session_end(
        &mut self,
        id: &DesignSessionId,
        reason: &str,
    ) -> Result<Option<DesignSession>, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        match session_by_id(&tx, id)? {
            Some(s) if s.state != SessionState::Ended => {}
            _ => return Ok(None),
        }
        end_session(&tx, id, reason)?;
        let ended = session_by_id(&tx, id)?;
        tx.commit().ctx("commit design session end")?;
        Ok(ended)
    }

    /// Add a note to the item's current, live session. A repeated `client_key` returns
    /// the stored row and `false`.
    pub fn design_note_add(
        &mut self,
        new: NewDesignNote,
    ) -> Result<(DesignNote, bool), ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let added = insert_note(&tx, new)?;
        tx.commit().ctx("commit design note")?;
        Ok(added)
    }

    /// Queue a Continue's reverts and follow-up together, including their sequence numbers.
    pub fn design_notes_add_group(
        &mut self,
        group: Vec<NewDesignNote>,
    ) -> Result<Vec<DesignNote>, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let mut notes = Vec::with_capacity(group.len());
        for new in group {
            let (note, created) = insert_note(&tx, new)?;
            if !created {
                return Err(ArchonError::Invalid(
                    "a different note already uses this continue key".into(),
                ));
            }
            notes.push(note);
        }
        tx.commit().ctx("commit design continue group")?;
        Ok(notes)
    }

    pub fn design_note(
        &self,
        item: &QueueItemId,
        id: &str,
    ) -> Result<Option<DesignNote>, ArchonError> {
        note_by_id(&self.conn, item, id)
    }

    /// The id the item's next note would take, from its never-decremented
    /// counter and every id its approved intent already uses; nothing is
    /// reserved. A design finished with no change names its requirement so.
    pub fn design_next_note_id(&self, item: &QueueItemId) -> Result<String, ArchonError> {
        let seq = settings::kv_get(&self.conn, &keys::design_seq(item))?
            .and_then(|s| s.parse::<u64>().ok());
        let used = intent_ids(&self.conn, item)?;
        let used: Vec<&str> = used.iter().map(String::as_str).collect();
        Ok(note_id(next_note_number(seq, &used)))
    }

    /// Every note of the item, all sessions, by number.
    pub fn design_notes(&self, item: &QueueItemId) -> Result<Vec<DesignNote>, ArchonError> {
        read_notes(
            &self.conn,
            "WHERE item = ?1 ORDER BY number",
            &[&item.as_str()],
        )
    }

    pub fn design_note_change(
        &mut self,
        item: &QueueItemId,
        id: &str,
        change: NoteChange,
    ) -> Result<DesignNote, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let updated = Self::change_note(&tx, item, id, change)?;
        tx.commit().ctx("commit design note change")?;
        Ok(updated)
    }

    fn change_note(
        tx: &Connection,
        item: &QueueItemId,
        id: &str,
        change: NoteChange,
    ) -> Result<DesignNote, ArchonError> {
        let mut note = note_by_id(tx, item, id)?
            .ok_or_else(|| ArchonError::NotFound(format!("design note {id}")))?;
        let current = current_session(tx, item)?;
        if current.as_ref().map(|s| &s.id) != Some(&note.session) {
            return Err(refuse(
                DesignRefusal::ReadOnly,
                format!("{id} belongs to an earlier design session; add a new note instead"),
            ));
        }
        let (own_session, own_id) = (note.session.clone(), note.id.clone());
        let others_in_flight = |tx: &Connection| -> Result<Option<String>, ArchonError> {
            Ok(session_notes(tx, &own_session)?
                .into_iter()
                .find(|n| n.id != own_id && is_in_flight(n.status))
                .map(|n| n.id))
        };
        match change {
            NoteChange::Edit { text } => {
                if note.status != NoteStatus::Queued {
                    return Err(ArchonError::InvalidTransition(format!(
                        "{id} was already sent and can no longer be edited"
                    )));
                }
                let text = clean_text(&text, MAX_NOTE_CHARS);
                if text.is_empty() {
                    return Err(ArchonError::Invalid("a note needs words".into()));
                }
                note.text = text;
            }
            NoteChange::Sent {
                base_commit,
                delivery,
            } => {
                if !transition_allowed(note.kind, note.status, NoteStatus::Sent) {
                    return Err(ArchonError::InvalidTransition(format!(
                        "{id} cannot be sent from {}",
                        enc(&note.status)
                    )));
                }
                if let Some(other) = others_in_flight(tx)? {
                    return Err(ArchonError::InvalidTransition(format!(
                        "{other} is still with the agent; {id} waits"
                    )));
                }
                note.status = NoteStatus::Sent;
                note.base_commit = Some(base_commit);
                note.delivery = delivery;
            }
            NoteChange::Redelivered { delivery } => {
                if note.status != NoteStatus::Sent {
                    return Err(ArchonError::InvalidTransition(format!(
                        "{id} is not waiting for delivery"
                    )));
                }
                note.delivery = delivery;
            }
            NoteChange::Status {
                to,
                fail_reason,
                commit,
                no_change,
            } => {
                if !transition_allowed(note.kind, note.status, to) {
                    return Err(ArchonError::InvalidTransition(format!(
                        "{id} cannot go from {} to {}",
                        enc(&note.status),
                        enc(&to)
                    )));
                }
                if note.status == NoteStatus::Queued
                    && is_in_flight(to)
                    && let Some(other) = others_in_flight(tx)?
                {
                    return Err(ArchonError::InvalidTransition(format!(
                        "{other} is still in flight; {id} waits"
                    )));
                }
                note.status = to;
                if fail_reason.is_some() {
                    note.fail_reason = fail_reason;
                }
                if commit.is_some() {
                    note.commit = commit;
                }
                note.no_change = no_change;
            }
            NoteChange::RevertPrepared {
                base_commit,
                commit,
            } => {
                if note.kind != NoteKind::Revert
                    || !transition_allowed(note.kind, note.status, NoteStatus::Working)
                {
                    return Err(ArchonError::InvalidTransition(format!(
                        "{id} is not a revert waiting to run"
                    )));
                }
                if let Some(other) = others_in_flight(tx)? {
                    return Err(ArchonError::InvalidTransition(format!(
                        "{other} is still in flight; {id} waits"
                    )));
                }
                note.status = NoteStatus::Working;
                note.base_commit = Some(base_commit);
                note.commit = Some(commit);
            }
            NoteChange::Thread(entry) => note.thread.push(entry),
            NoteChange::Before { asset } => note.before_asset = Some(asset),
            NoteChange::After {
                asset,
                captured_commit,
            } => {
                note.after_asset = Some(asset);
                note.captured_commit = Some(captured_commit);
            }
            NoteChange::Final {
                asset,
                included,
                captured_commit,
            } => {
                note.final_asset = asset;
                note.final_included = included;
                note.captured_commit = captured_commit;
            }
            NoteChange::Superseded { by } => note.superseded_by = by,
        }
        let at = now();
        tx.execute(
            "UPDATE design_notes SET text = ?3, status = ?4, fail_reason = ?5, base_commit = ?6,
                 commit_sha = ?7, no_change = ?8, superseded_by = ?9, reply_thread_json = ?10,
                 delivery = ?11, after_asset = ?12, final_asset = ?13, final_included = ?14,
                 captured_commit = ?15, updated_at = ?16, before_asset = ?17
             WHERE item = ?1 AND id = ?2",
            params![
                item.as_str(),
                id,
                note.text,
                enc(&note.status),
                note.fail_reason.as_ref().map(enc),
                note.base_commit,
                note.commit,
                note.no_change,
                note.superseded_by,
                to_json(&note.thread, "note thread")?,
                i64::from(note.delivery),
                note.after_asset.as_ref().map(DesignAssetId::as_str),
                note.final_asset.as_ref().map(DesignAssetId::as_str),
                note.final_included,
                note.captured_commit,
                at,
                note.before_asset.as_ref().map(DesignAssetId::as_str),
            ],
        )
        .ctx("update design note")?;
        bump(tx, &note.session)?;
        let updated = note_by_id(tx, item, id)?.expect("the note exists");
        Ok(updated)
    }

    /// Store a picture's record; the one already held for (item, sha256, source) is returned
    /// instead of a second.
    pub fn design_asset_put(&mut self, asset: DesignAsset) -> Result<DesignAsset, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let source = enc(&asset.source);
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM design_assets WHERE item = ?1 AND sha256 = ?2 AND source = ?3",
                params![asset.item.as_str(), asset.sha256, source],
                |r| r.get(0),
            )
            .optional()
            .ctx("find design asset")?;
        if let Some(existing) = existing {
            let found = asset_by_id(&tx, &asset.item, &DesignAssetId::parse(&existing)?)?
                .expect("the asset was just found");
            return Ok(found);
        }
        tx.execute(
            "INSERT INTO design_assets (id, item, sha256, mime, bytes, width, height, path,
                 source, target_json, note, commit_sha, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                asset.id.as_str(),
                asset.item.as_str(),
                asset.sha256,
                asset.mime,
                to_i64(asset.bytes, "asset bytes")?,
                i64::from(asset.width),
                i64::from(asset.height),
                asset.path,
                source,
                asset
                    .target
                    .as_ref()
                    .map(|t| to_json(t, "asset target"))
                    .transpose()?,
                asset.note,
                asset.commit,
                ts(&asset.created_at),
            ],
        )
        .ctx("insert design asset")?;
        let stored =
            asset_by_id(&tx, &asset.item, &asset.id)?.expect("the asset was just inserted");
        tx.commit().ctx("commit design asset")?;
        Ok(stored)
    }

    pub fn design_asset(
        &self,
        item: &QueueItemId,
        id: &DesignAssetId,
    ) -> Result<Option<DesignAsset>, ArchonError> {
        asset_by_id(&self.conn, item, id)
    }

    /// The bytes the item's pictures hold.
    pub fn design_asset_bytes(&self, item: &QueueItemId) -> Result<u64, ArchonError> {
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(bytes), 0) FROM design_assets WHERE item = ?1",
                [item.as_str()],
                |r| r.get(0),
            )
            .ctx("sum design asset bytes")?;
        to_u64(total, "asset bytes")
    }

    /// Pictures created before `older_than` that no note uses.
    pub fn design_unused_assets(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<Vec<DesignAsset>, ArchonError> {
        read_assets(
            &self.conn,
            "WHERE created_at < ?1 AND NOT EXISTS (
                 SELECT 1 FROM design_notes n WHERE n.item = design_assets.item
                   AND design_assets.id IN (n.before_asset, n.after_asset, n.final_asset))
             ORDER BY created_at",
            &[&ts(&older_than)],
        )
    }

    pub fn design_asset_delete(
        &mut self,
        item: &QueueItemId,
        id: &DesignAssetId,
    ) -> Result<(), ArchonError> {
        self.conn
            .execute(
                "DELETE FROM design_assets WHERE item = ?1 AND id = ?2",
                params![item.as_str(), id.as_str()],
            )
            .ctx("delete design asset")?;
        Ok(())
    }

    /// Whether another record shares this picture file.
    pub fn design_asset_sha_shared(
        &self,
        item: &QueueItemId,
        hash: &str,
        id: &DesignAssetId,
    ) -> Result<bool, ArchonError> {
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM design_assets WHERE item = ?1 AND sha256 = ?2 AND id != ?3)",
            params![item.as_str(), hash, id.as_str()], |row| row.get(0),
        ).ctx("check shared design picture")
    }

    /// Remove everything the item holds; the pictures come back so their files can go.
    pub fn design_item_delete(
        &mut self,
        item: &QueueItemId,
    ) -> Result<Vec<DesignAsset>, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let assets = read_assets(
            &tx,
            "WHERE item = ?1 ORDER BY created_at",
            &[&item.as_str()],
        )?;
        for session in read_sessions(&tx, "WHERE item = ?1", &[&item.as_str()])? {
            settings::kv_delete(&tx, &keys::design_owed(&session.id))?;
        }
        for table in ["design_notes", "design_sessions", "design_assets"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE item = ?1"),
                [item.as_str()],
            )
            .ctx("delete design rows")?;
        }
        settings::kv_delete(&tx, &keys::design_seq(item))?;
        tx.commit().ctx("commit design item delete")?;
        Ok(assets)
    }
}

fn check_note_capacity(tx: &Connection, session: &DesignSessionId) -> Result<(), ArchonError> {
    let count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM design_notes WHERE session = ?1",
            [session.as_str()],
            |r| r.get(0),
        )
        .ctx("count design notes")?;
    if count >= MAX_NOTES_PER_SESSION as i64 {
        return Err(ArchonError::Invalid(format!(
            "a session holds at most {MAX_NOTES_PER_SESSION} notes"
        )));
    }
    Ok(())
}

fn insert_note(tx: &Connection, new: NewDesignNote) -> Result<(DesignNote, bool), ArchonError> {
    if let Some(found) = read_notes(
        tx,
        "WHERE item = ?1 AND client_key = ?2",
        &[&new.item.as_str(), &new.client_key],
    )?
    .pop()
    {
        return Ok((found, false));
    }
    let current = current_session(tx, &new.item)?;
    let session = match current {
        Some(s) if s.id == new.session => s,
        _ => return Err(refuse(DesignRefusal::Gone, "this design session has ended")),
    };
    match session.state {
        SessionState::Live => {}
        SessionState::Finishing => {
            return Err(refuse(
                DesignRefusal::Finishing,
                "the session is being finished; go back to editing first",
            ));
        }
        _ => {
            return Err(refuse(
                DesignRefusal::Gone,
                "this design session is not live",
            ));
        }
    }
    if new.text.trim().is_empty() {
        return Err(ArchonError::Invalid("a note needs words".into()));
    }
    check_note_capacity(tx, &new.session)?;
    write_note(tx, new)
}
fn write_note(tx: &Connection, new: NewDesignNote) -> Result<(DesignNote, bool), ArchonError> {
    let text = clean_text(&new.text, MAX_NOTE_CHARS);
    if text.is_empty() {
        return Err(ArchonError::Invalid("a note needs words".into()));
    }
    let seq_key = keys::design_seq(&new.item);
    let seq = settings::kv_get(tx, &seq_key)?
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|_| ArchonError::Corrupt("invalid stored design note number".into()))?;
    let used = intent_ids(tx, &new.item)?;
    let used: Vec<&str> = used.iter().map(String::as_str).collect();
    let number = next_note_number(seq, &used);
    let id = note_id(number);
    let at = now();
    tx.execute(
        "INSERT INTO design_notes (item, id, number, session, kind, text, details_text,
                 send_picture, target_json, viewport_json, status, reply_thread_json, client_key,
                 reverts, resent_from, before_asset, created_by_device, created_at, updated_at,
                 follows, refs_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'queued', '[]', ?11, ?12, ?13,
                 ?14, ?15, ?16, ?16, ?17, ?18)",
        params![
            new.item.as_str(),
            id,
            to_i64(number, "note number")?,
            new.session.as_str(),
            enc(&new.kind),
            text,
            new.details_text
                .as_deref()
                .map(|d| clean_text(d, archon_core::design::MAX_DETAILS_CHARS)),
            new.send_picture,
            new.target
                .as_ref()
                .map(|t| to_json(t, "note target"))
                .transpose()?,
            to_json(&new.viewport, "note viewport")?,
            new.client_key,
            new.reverts,
            new.resent_from,
            new.before_asset.as_ref().map(DesignAssetId::as_str),
            new.created_by_device,
            at,
            new.follows,
            to_json(&new.refs, "note refs")?,
        ],
    )
    .ctx("insert design note")?;
    settings::kv_set(tx, &seq_key, &(number + 1).to_string())?;
    bump(tx, &new.session)?;
    let note = note_by_id(tx, &new.item, &id)?.expect("the note was just inserted");
    Ok((note, true))
}

const ASSET_COLUMNS: &str = "id, item, sha256, mime, bytes, width, height, path, source,
    target_json, note, commit_sha, created_at";

fn read_assets(
    conn: &Connection,
    clause: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<DesignAsset>, ArchonError> {
    let sql = format!("SELECT {ASSET_COLUMNS} FROM design_assets {clause}");
    let mut statement = conn.prepare_cached(&sql).ctx("prepare design assets")?;
    let rows = statement
        .query_map(args, |row| {
            let item: String = row.get(1)?;
            let bytes: i64 = row.get(4)?;
            let width: i64 = row.get(5)?;
            let height: i64 = row.get(6)?;
            let source: String = row.get(8)?;
            let target: Option<String> = row.get(9)?;
            let created: String = row.get(12)?;
            Ok((|| -> Result<DesignAsset, ArchonError> {
                fn ctx<T>(r: rusqlite::Result<T>) -> Result<T, ArchonError> {
                    r.ctx("read design asset")
                }
                Ok(DesignAsset {
                    id: DesignAssetId::try_from(ctx(row.get::<_, String>(0))?)?,
                    item: QueueItemId::parse(&item)?,
                    sha256: ctx(row.get(2))?,
                    mime: ctx(row.get(3))?,
                    bytes: to_u64(bytes, "asset bytes")?,
                    width: u32::try_from(width).map_err(corrupt("asset width"))?,
                    height: u32::try_from(height).map_err(corrupt("asset height"))?,
                    path: ctx(row.get(7))?,
                    source: dec(&source, "asset source")?,
                    target: target.map(|t| from_json(&t, "asset target")).transpose()?,
                    note: ctx(row.get(10))?,
                    commit: ctx(row.get(11))?,
                    created_at: parse_ts(&created)?,
                })
            })())
        })
        .ctx("read design assets")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.ctx("read design assets")??);
    }
    Ok(out)
}

fn asset_by_id(
    conn: &Connection,
    item: &QueueItemId,
    id: &DesignAssetId,
) -> Result<Option<DesignAsset>, ArchonError> {
    Ok(read_assets(
        conn,
        "WHERE item = ?1 AND id = ?2",
        &[&item.as_str(), &id.as_str()],
    )?
    .pop())
}
