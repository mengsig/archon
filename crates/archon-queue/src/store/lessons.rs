//! Bounded lessons, operator precedence, stable use snapshots and outcome scoring.
mod scoring;
mod selection;

use super::Store;
use super::codec::{SqlContext, from_json, to_json, ts};
use archon_core::error::ArchonError;
use archon_core::id::{LessonId, RepoId};
use archon_core::lesson::{Lesson, LessonPatch, LessonQuery, LessonSource, LessonState};
use rusqlite::{Connection, OptionalExtension, params};
pub(super) use scoring::score_run;

const MAX_SCOPE_LESSONS: usize = 100;

impl Store {
    pub fn lesson(&self, id: &LessonId) -> Result<Lesson, ArchonError> {
        load(&self.conn, id)
    }

    pub fn lessons(&self, query: &LessonQuery) -> Result<Vec<Lesson>, ArchonError> {
        let repo = query.repo.as_ref().map(|r| self.repo(r)).transpose()?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT data FROM lessons WHERE superseded_by IS NULL
            AND (?1 IS NULL OR repo=?1 OR repo IS NULL) AND (?2 IS NULL OR state=?2)
            AND (?3 IS NULL OR kind=?3) ORDER BY repo IS NULL,confidence DESC,id",
            )
            .ctx("prepare lessons")?;
        let rows = stmt
            .query_map(
                params![
                    repo.as_ref().map(|r| r.id.as_str()),
                    query.state.map(|s| s.as_str()),
                    query.kind.map(|k| k.as_str())
                ],
                |r| r.get::<_, String>(0),
            )
            .ctx("query lessons")?;
        rows.map(|r| from_json(&r.ctx("read lesson")?, "lesson"))
            .collect()
    }

    pub fn record_lesson(&self, mut lesson: Lesson) -> Result<Lesson, ArchonError> {
        require_enabled(self)?;
        lesson.text = archon_core::lesson::sanitized_text(&lesson.text)?;
        let tx = self.conn.unchecked_transaction().ctx("begin lesson")?;
        let existing = scoped(&tx, lesson.scope.repo.as_ref())?;
        if let Some(known) = existing
            .iter()
            .find(|l| l.scope == lesson.scope && normalized(&l.text) == normalized(&lesson.text))
        {
            assert_eq!(known.scope, lesson.scope);
            if known.source == LessonSource::Operator || lesson.source != LessonSource::Operator {
                return Ok(known.clone());
            }
            lesson.id = known.id.clone();
            lesson.created_at = known.created_at;
            lesson.uses = known.uses;
            lesson.wins = known.wins;
            lesson.confidence = known.confidence;
        } else if existing.len() >= MAX_SCOPE_LESSONS {
            return Err(ArchonError::Invalid(format!(
                "lesson scope limit is {MAX_SCOPE_LESSONS}; edit existing lessons"
            )));
        }
        assert!(lesson.wins <= lesson.uses);
        write(&tx, &lesson)?;
        tx.commit().ctx("commit lesson")?;
        Ok(lesson)
    }

    pub fn update_lesson(&self, id: &LessonId, patch: LessonPatch) -> Result<Lesson, ArchonError> {
        require_enabled(self)?;
        if patch.is_empty() {
            return Err(ArchonError::Invalid("lesson patch is empty".into()));
        }
        let tx = self.conn.unchecked_transaction().ctx("begin lesson edit")?;
        let reset = patch.text.is_some()
            || patch.kind.is_some()
            || patch.state == Some(LessonState::Active);
        let mut lesson = load(&tx, id)?;
        if let Some(text) = patch.text {
            lesson.text = archon_core::lesson::sanitized_text(&text)?;
        }
        if let Some(kind) = patch.kind {
            lesson.kind = kind;
        }
        if let Some(pinned) = patch.pinned {
            lesson.pinned = pinned;
        }
        if let Some(state) = patch.state {
            lesson.state = state;
        }
        lesson.source = LessonSource::Operator;
        lesson.updated_at = archon_core::now();
        lesson.confirmed_at = Some(lesson.updated_at);
        lesson.confidence = 1.0;
        if reset {
            lesson.uses = 0;
            lesson.wins = 0;
            super::settings::kv_set(
                &tx,
                &format!("learning/lesson-epoch/{id}"),
                &ts(&lesson.updated_at),
            )?;
        }
        accept_rewrite(&tx, &lesson)?;
        merge_duplicates(&tx, &lesson)?;
        write(&tx, &lesson)?;
        assert_eq!(lesson.source, LessonSource::Operator);
        assert!(lesson.confirmed_at.is_some());
        tx.commit().ctx("commit lesson edit")?;
        Ok(lesson)
    }

    pub fn refresh_lessons(&self, at: archon_core::Timestamp) -> Result<(), ArchonError> {
        if !self.learning_config()?.enabled {
            return Ok(());
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("begin lesson decay")?;
        for mut lesson in scoped_all(&tx)? {
            scoring::refresh(&mut lesson, at);
            write(&tx, &lesson)?;
        }
        tx.commit().ctx("commit lesson decay")
    }
}

fn require_enabled(store: &Store) -> Result<(), ArchonError> {
    if !store.learning_config()?.enabled {
        return Err(ArchonError::Invalid("learning is disabled".into()));
    }
    Ok(())
}

fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn scoped(conn: &Connection, repo: Option<&RepoId>) -> Result<Vec<Lesson>, ArchonError> {
    let mut stmt = conn
        .prepare("SELECT data FROM lessons WHERE repo IS ?1 AND superseded_by IS NULL")
        .ctx("prepare scoped lessons")?;
    let rows = stmt
        .query_map([repo.map(RepoId::as_str)], |r| r.get::<_, String>(0))
        .ctx("query scoped lessons")?;
    rows.map(|r| from_json(&r.ctx("read scoped lesson")?, "lesson"))
        .collect()
}

fn scoped_all(conn: &Connection) -> Result<Vec<Lesson>, ArchonError> {
    let mut stmt = conn
        .prepare("SELECT data FROM lessons WHERE state='active' AND superseded_by IS NULL")
        .ctx("prepare active lessons")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .ctx("query active lessons")?;
    rows.map(|r| from_json(&r.ctx("read active lesson")?, "lesson"))
        .collect()
}

fn load(conn: &Connection, id: &LessonId) -> Result<Lesson, ArchonError> {
    let raw: Option<String> = conn
        .query_row("SELECT data FROM lessons WHERE id=?1", [id.as_str()], |r| {
            r.get(0)
        })
        .optional()
        .ctx("read lesson")?;
    from_json(
        &raw.ok_or_else(|| ArchonError::NotFound(format!("lesson {id}")))?,
        "lesson",
    )
}

fn merge_duplicates(conn: &Connection, lesson: &Lesson) -> Result<(), ArchonError> {
    assert!(lesson.superseded_by.is_none());
    for mut other in scoped(conn, lesson.scope.repo.as_ref())? {
        if other.id == lesson.id
            || other.scope != lesson.scope
            || normalized(&other.text) != normalized(&lesson.text)
        {
            continue;
        }
        assert_ne!(other.id, lesson.id);
        other.superseded_by = Some(lesson.id.clone());
        other.state = LessonState::Retracted;
        write(conn, &other)?;
    }
    Ok(())
}

fn write(conn: &Connection, l: &Lesson) -> Result<(), ArchonError> {
    assert!(l.wins <= l.uses);
    assert!(l.confidence.is_finite() && (0.0..=1.0).contains(&l.confidence));
    conn.execute("INSERT INTO lessons (id,repo,pipeline,kind,state,pinned,uses,wins,confidence,source,
        superseded_by,created_at,updated_at,confirmed_at,data)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
        ON CONFLICT(id) DO UPDATE SET kind=excluded.kind,state=excluded.state,pinned=excluded.pinned,
        uses=excluded.uses,wins=excluded.wins,confidence=excluded.confidence,source=excluded.source,
        superseded_by=excluded.superseded_by,updated_at=excluded.updated_at,
        confirmed_at=excluded.confirmed_at,data=excluded.data",
        params![l.id.as_str(),l.scope.repo.as_ref().map(RepoId::as_str),l.scope.pipeline,l.kind.as_str(),
            l.state.as_str(),l.pinned,l.uses,l.wins,l.confidence,l.source.as_str(),
            l.superseded_by.as_ref().map(LessonId::as_str),ts(&l.created_at),ts(&l.updated_at),
            l.confirmed_at.as_ref().map(ts),to_json(l,"lesson")?]).ctx("write lesson")?;
    Ok(())
}

impl Store {
    pub fn record_distilled_lesson(
        &self,
        lesson: Lesson,
        replaces: Option<&LessonId>,
    ) -> Result<Lesson, ArchonError> {
        assert_eq!(lesson.source, LessonSource::Distilled);
        assert_eq!(lesson.state, LessonState::Proposed);
        if let Some(id) = replaces {
            let prior = self.lesson(id)?;
            if prior.scope.repo != lesson.scope.repo {
                return Err(ArchonError::Invalid(
                    "a rewrite keeps its repository scope".into(),
                ));
            }
            if prior.source == LessonSource::Operator || prior.pinned {
                return Ok(prior);
            }
        }
        let recorded = self.record_lesson(lesson)?;
        if let Some(id) = replaces
            && *id != recorded.id
        {
            self.save_learning_value(&format!("learning/rewrite/{}", recorded.id), id.as_str())?;
        }
        Ok(recorded)
    }
}

fn accept_rewrite(conn: &Connection, lesson: &Lesson) -> Result<(), ArchonError> {
    if lesson.state != LessonState::Active {
        return Ok(());
    }
    let Some(raw) = super::settings::kv_get(conn, &format!("learning/rewrite/{}", lesson.id))?
    else {
        return Ok(());
    };
    let mut old = load(conn, &LessonId::parse(&raw)?)?;
    assert_ne!(old.id, lesson.id);
    assert_eq!(old.scope.repo, lesson.scope.repo);
    if old.source == LessonSource::Operator || old.pinned {
        return Ok(());
    }
    old.state = LessonState::Retracted;
    old.superseded_by = Some(lesson.id.clone());
    write(conn, &old)
}
