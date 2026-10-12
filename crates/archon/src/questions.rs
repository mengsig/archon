//! Open questions as the operator refers to them: by the permanent number
//! archond gave each one (`Q7` everywhere, never reused) or by id, with
//! numbered options.

pub use archon_core::notify::QuestionRef;
use archon_core::notify::{FeedEntry, Question};

use crate::render::QuestionLabels;

/// The open question `reference` names, if it is still open. A number
/// matches only the question archond gave it, so a question that closed
/// since it was listed is not found rather than replaced by another.
pub fn find_open<'a>(open: &'a [Question], reference: &QuestionRef) -> Option<&'a Question> {
    let found = open.iter().find(|q| match reference {
        QuestionRef::Number(n) => q.number == *n,
        QuestionRef::Id(id) => &q.id == id,
    });
    debug_assert!(found.is_none_or(Question::is_open));
    found
}

#[derive(Debug)]
struct Entry {
    question: Question,
    open: bool,
}

/// The questions the chat has seen this session and which are still open,
/// under archond's numbers, so `Q3` in the chat is `Q3` in `archon
/// questions` too.
#[derive(Debug, Default)]
pub struct QuestionBook {
    labels: QuestionLabels,
    entries: Vec<Entry>,
}

impl QuestionBook {
    pub fn labels(&self) -> &QuestionLabels {
        &self.labels
    }

    /// Adopt the daemon's list of open questions: new ones are added,
    /// known ones missing from it are closed.
    pub fn sync_open(&mut self, open: Vec<Question>) {
        for entry in &mut self.entries {
            entry.open = open.iter().any(|q| q.id == entry.question.id);
        }
        for question in open.into_iter().filter(Question::is_open) {
            self.add(question);
        }
    }

    /// Track a live feed entry. Only live entries: a replayed question may
    /// have been answered since, and would wrongly get a number.
    pub fn observe(&mut self, entry: &FeedEntry) {
        match entry {
            FeedEntry::Question(question) if question.is_open() => self.add(question.clone()),
            FeedEntry::Answer { question, .. } => {
                if let Some(entry) = self.entries.iter_mut().find(|e| &e.question.id == question) {
                    entry.open = false;
                }
            }
            _ => {}
        }
    }

    /// The most recently asked question still open, for `/a`.
    pub fn newest_open(&self) -> Option<&Question> {
        self.open()
            .max_by(|a, b| a.asked_at.cmp(&b.asked_at).then(a.number.cmp(&b.number)))
    }

    pub fn find(&self, reference: &QuestionRef) -> Option<&Question> {
        self.entries
            .iter()
            .map(|e| &e.question)
            .find(|q| match reference {
                QuestionRef::Number(n) => q.number == *n,
                QuestionRef::Id(id) => &q.id == id,
            })
    }

    /// Open questions by number.
    pub fn open(&self) -> impl Iterator<Item = &Question> {
        self.entries.iter().filter(|e| e.open).map(|e| &e.question)
    }

    /// A question without a number (from an older archond) is left out:
    /// the operator could not name it by number anyway.
    fn add(&mut self, question: Question) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.question.id == question.id)
        {
            entry.open = question.is_open();
            return;
        }
        if question.number == 0 {
            return;
        }
        self.labels.insert(question.id.clone(), question.number);
        let at = self
            .entries
            .partition_point(|e| e.question.number < question.number);
        self.entries.insert(
            at,
            Entry {
                open: question.is_open(),
                question,
            },
        );
        debug_assert!(
            self.entries
                .windows(2)
                .all(|w| w[0].question.number < w[1].question.number),
            "archond gives each question its own number"
        );
    }
}

#[cfg(test)]
mod tests {
    use archon_core::id::QuestionId;
    use archon_core::notify::AnswerVia;
    use chrono::Duration;

    use super::*;

    fn question(number: u64, text: &str, age_secs: i64, options: &[&str]) -> Question {
        Question {
            id: QuestionId::new(),
            number,
            text: text.into(),
            options: options.iter().map(|o| (*o).to_owned()).collect(),
            item: None,
            aether: None,
            asked_at: archon_core::now() - Duration::seconds(age_secs),
            answer: None,
            answered_at: None,
        }
    }

    #[test]
    fn a_listed_number_never_names_a_different_question() {
        let gate_b = question(1, "Approve gate B", 30, &["yes", "reject"]);
        let gate_c = question(2, "Approve gate C", 20, &["yes", "reject"]);
        let listed = [gate_b.clone(), gate_c.clone()];
        assert_eq!(find_open(&listed, &QuestionRef::Number(2)), Some(&gate_c));
        // Gate B is decided elsewhere and gate D is asked before `answer 2`.
        let gate_d = question(3, "Approve gate D", 0, &["yes", "reject"]);
        let now_open = [gate_c.clone(), gate_d.clone()];
        assert_eq!(find_open(&now_open, &QuestionRef::Number(2)), Some(&gate_c));
        assert_eq!(find_open(&now_open, &QuestionRef::Number(1)), None);
        assert_eq!(find_open(&now_open, &QuestionRef::Number(3)), Some(&gate_d));
        let by_id = QuestionRef::Id(gate_d.id.clone());
        assert_eq!(find_open(&now_open, &by_id), Some(&gate_d));
        assert_eq!(find_open(&now_open, &QuestionRef::Id(gate_b.id)), None);
    }

    #[test]
    fn the_chat_uses_archond_numbers_and_tracks_what_is_open() {
        let old = question(4, "old", 60, &[]);
        let new = question(9, "new", 5, &[]);
        let mut book = QuestionBook::default();
        book.sync_open(vec![new.clone(), old.clone()]);
        assert_eq!(book.find(&QuestionRef::Number(4)).unwrap().text, "old");
        assert!(book.find(&QuestionRef::Number(1)).is_none());
        assert_eq!(book.newest_open().unwrap().text, "new");
        book.observe(&FeedEntry::Answer {
            question: new.id.clone(),
            text: "ok".into(),
            via: AnswerVia::Archon,
        });
        assert_eq!(book.newest_open().unwrap().number, 4);
        let later = question(10, "later", 0, &[]);
        book.observe(&FeedEntry::Question(later.clone()));
        assert_eq!(book.labels().get(&later.id), Some(10));
        book.sync_open(vec![later]);
        assert_eq!(book.open().map(|q| q.number).collect::<Vec<_>>(), [10]);
    }

    #[test]
    fn open_questions_list_by_number_and_unnumbered_ones_are_skipped() {
        let mut book = QuestionBook::default();
        let live = question(12, "live", 0, &[]);
        book.observe(&FeedEntry::Question(live.clone()));
        let unnumbered = question(0, "from an older archond", 10, &[]);
        let synced = question(11, "synced", 5, &[]);
        book.sync_open(vec![live, synced, unnumbered.clone()]);
        assert_eq!(book.open().map(|q| q.number).collect::<Vec<_>>(), [11, 12]);
        assert_eq!(book.labels().get(&unnumbered.id), None);
    }
}
