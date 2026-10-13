//! Text for a recorded lesson.

use archon_core::lesson::Lesson;

pub(super) fn recorded(lesson: &Lesson) -> String {
    let scope = match (&lesson.scope.repo, &lesson.scope.pipeline) {
        (None, None) => "everywhere".to_owned(),
        (Some(repo), None) => format!("repo {repo}"),
        (None, Some(pipeline)) => format!("pipeline {pipeline}"),
        (Some(repo), Some(pipeline)) => format!("repo {repo}, pipeline {pipeline}"),
    };
    format!(
        "Recorded lesson {} [{}, {}] for {scope}: {}",
        lesson.id,
        lesson.kind.as_str(),
        lesson.state.as_str(),
        lesson.text
    )
}

#[cfg(test)]
mod tests {
    use archon_core::lesson::{LessonKind, LessonScope, LessonSource, LessonState};

    use super::*;

    #[test]
    fn a_recorded_lesson_names_its_id_state_and_scope() {
        let scope = LessonScope {
            repo: None,
            pipeline: Some("quick-pr".into()),
        };
        let (kind, source, state) = (
            LessonKind::Pitfall,
            LessonSource::Orchestrator,
            LessonState::Proposed,
        );
        let lesson = Lesson::new(
            scope,
            kind,
            "Run `make lint`.",
            source,
            state,
            archon_core::now(),
        )
        .unwrap();
        let text = recorded(&lesson);
        assert!(text.starts_with(&format!(
            "Recorded lesson {} [pitfall, proposed]",
            lesson.id
        )));
        assert!(text.ends_with("for pipeline quick-pr: Run `make lint`."));
    }
}
