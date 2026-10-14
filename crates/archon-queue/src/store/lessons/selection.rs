use super::super::Store;
use super::super::codec::{SqlContext, from_json, to_json, ts};
use super::super::settings;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, RepoId};
use archon_core::lesson::{CONTRACT_LESSON_BYTES, CONTRACT_LESSONS, Lesson, LessonState};
use rusqlite::params;

impl Store {
    pub fn lessons_for_run(
        &self,
        run: &PipelineRunId,
        repo: Option<&RepoId>,
        pipeline: &str,
    ) -> Result<Vec<Lesson>, ArchonError> {
        if !self.learning_config()?.enabled {
            return Ok(Vec::new());
        }
        let key = format!("learning/uses/{run}");
        if let Some(raw) = self.kv_get(&key)? {
            return from_json(&raw, "lesson snapshot");
        }
        let stored = self.run(run)?;
        assert_eq!(stored.pipeline, pipeline);
        assert_eq!(self.item(&stored.item)?.repo.as_ref(), repo);
        self.refresh_lessons(archon_core::now())?;
        let lessons = self.select_lessons(
            repo,
            Some(pipeline),
            CONTRACT_LESSONS,
            CONTRACT_LESSON_BYTES,
        )?;
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("begin lesson snapshot")?;
        for lesson in &lessons {
            tx.execute(
                "INSERT OR IGNORE INTO lesson_uses (lesson,run,used_at) VALUES (?1,?2,?3)",
                params![lesson.id.as_str(), run.as_str(), ts(&archon_core::now())],
            )
            .ctx("record lesson use")?;
        }
        settings::kv_set(&tx, &key, &to_json(&lessons, "lesson snapshot")?)?;
        tx.commit().ctx("commit lesson snapshot")?;
        Ok(lessons)
    }

    pub fn used_lessons(&self, run: &PipelineRunId) -> Result<Vec<Lesson>, ArchonError> {
        self.kv_get(&format!("learning/uses/{run}"))?
            .map_or_else(|| Ok(Vec::new()), |raw| from_json(&raw, "lesson snapshot"))
    }

    pub fn select_lessons(
        &self,
        repo: Option<&RepoId>,
        pipeline: Option<&str>,
        count: usize,
        bytes: usize,
    ) -> Result<Vec<Lesson>, ArchonError> {
        if count > 10 || bytes > 4096 {
            return Err(ArchonError::Invalid(
                "lesson selection exceeds contract bounds".into(),
            ));
        }
        let mut candidates = super::scoped(&self.conn, repo)?;
        if repo.is_some() {
            candidates.extend(super::scoped(&self.conn, None)?);
        }
        candidates.retain(|l| {
            l.state == LessonState::Active
                && l.scope
                    .pipeline
                    .as_deref()
                    .is_none_or(|p| Some(p) == pipeline)
        });
        candidates.sort_by(|a, b| {
            a.scope
                .repo
                .is_none()
                .cmp(&b.scope.repo.is_none())
                .then_with(|| b.scope.pipeline.is_some().cmp(&a.scope.pipeline.is_some()))
                .then_with(|| b.confidence.total_cmp(&a.confidence))
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut chosen = Vec::new();
        let mut total = archon_core::lesson::CONTRACT_LESSON_NOTICE.len();
        for lesson in candidates {
            if chosen.len() == count {
                break;
            }
            let rendered_bytes = lesson.quoted_observation().len();
            if total + rendered_bytes > bytes {
                continue;
            }
            total += rendered_bytes;
            chosen.push(lesson);
        }
        assert!(chosen.len() <= count);
        assert!(chosen.is_empty() || total <= bytes);
        Ok(chosen)
    }
}

impl Store {
    pub fn orchestrator_lessons(&self) -> Result<Vec<Lesson>, ArchonError> {
        if !self.learning_config()?.enabled {
            return Ok(Vec::new());
        }
        let mut candidates = self.lessons(&archon_core::lesson::LessonQuery {
            state: Some(LessonState::Active),
            ..Default::default()
        })?;
        candidates.sort_by(|a, b| {
            b.confidence
                .total_cmp(&a.confidence)
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut total = 0;
        let chosen: Vec<_> = candidates
            .into_iter()
            .filter(|l| {
                total += l.text.len();
                total <= 3000
            })
            .take(10)
            .collect();
        assert!(chosen.len() <= 10);
        assert!(chosen.iter().map(|l| l.text.len()).sum::<usize>() <= 3000);
        Ok(chosen)
    }
}
