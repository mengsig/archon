//! Learned advice stays bounded and cannot grant authority.
use archon_core::lesson::{
    CONTRACT_LESSON_BYTES, CONTRACT_LESSON_NOTICE, CONTRACT_LESSONS, Lesson,
};

pub fn lesson_rules(lessons: &[Lesson]) -> Result<String, archon_core::ArchonError> {
    assert!(lessons.len() <= CONTRACT_LESSONS);
    assert!(lessons.iter().map(|l| l.text.len()).sum::<usize>() <= CONTRACT_LESSON_BYTES);
    if lessons.is_empty() {
        return Ok(String::new());
    }
    let mut text = CONTRACT_LESSON_NOTICE.to_owned();
    for lesson in lessons {
        text.push_str(&lesson.quoted_observation()?);
    }
    assert!(text.len() <= CONTRACT_LESSON_BYTES);
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_run_without_lessons_is_told_nothing() {
        assert_eq!(lesson_rules(&[]).unwrap(), "");
    }
}
