//! The contract's Lessons section: what earlier runs taught, for the stage
//! agents of a later one.

use archon_core::lesson::Lesson;

/// At most `CONTRACT_LESSONS` lessons in `CONTRACT_LESSON_BYTES`; empty
/// without any. Not written yet: no lesson exists until one can be recorded.
pub fn lesson_rules(lessons: &[Lesson]) -> String {
    assert!(lessons.is_empty(), "lessons cannot be recorded yet");
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_without_lessons_is_told_nothing() {
        assert_eq!(lesson_rules(&[]), "");
    }
}
