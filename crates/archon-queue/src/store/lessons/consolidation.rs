//! Near-duplicate lesson clusters. A cluster groups lessons for display and so
//! a contract carries one of them; it never changes a lesson's text or standing.
use super::super::codec::SqlContext;
use archon_core::error::ArchonError;
use archon_core::id::{LessonId, RepoId};
use archon_core::lesson::{Lesson, LessonScope, LessonState};
use rusqlite::Connection;
use std::collections::BTreeSet;

/// Both similarities must reach these for two lessons to be near-duplicates.
const MIN_WORD_SIMILARITY: f64 = 0.6;
const MIN_TRIGRAM_SIMILARITY: f64 = 0.5;

/// Recomputes the clusters of `scope`'s lessons and writes those whose cluster changed.
pub(super) fn consolidate(conn: &Connection, scope: &LessonScope) -> Result<(), ArchonError> {
    let lessons: Vec<Lesson> = super::scoped(conn, scope.repo.as_ref())?
        .into_iter()
        .filter(|l| l.scope == *scope)
        .collect();
    let clusters = clusters(&lessons);
    assert_eq!(clusters.len(), lessons.len());
    for (mut lesson, cluster) in lessons.into_iter().zip(clusters) {
        if lesson.cluster == cluster {
            continue;
        }
        let standing = (lesson.state, lesson.uses, lesson.wins, lesson.confidence);
        lesson.cluster = cluster;
        assert_eq!(
            standing,
            (lesson.state, lesson.uses, lesson.wins, lesson.confidence)
        );
        super::write(conn, &lesson)?;
    }
    Ok(())
}

/// Consolidates every scope that has a lesson not superseded.
pub(super) fn consolidate_all(conn: &Connection) -> Result<(), ArchonError> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT repo, pipeline FROM lessons WHERE superseded_by IS NULL")
        .ctx("prepare lesson scopes")?;
    let scopes = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        })
        .ctx("query lesson scopes")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ctx("read lesson scopes")?;
    for (repo, pipeline) in scopes {
        let repo = repo.as_deref().map(RepoId::parse).transpose()?;
        consolidate(conn, &LessonScope { repo, pipeline })?;
    }
    Ok(())
}

/// Each lesson's cluster, in order: the smallest id of its group of transitive
/// near-duplicates, or `None` alone. Retracted lessons join no cluster.
fn clusters(lessons: &[Lesson]) -> Vec<Option<LessonId>> {
    let features: Vec<_> = lessons.iter().map(|l| Features::of(&l.text)).collect();
    let live = |i: usize| lessons[i].state != LessonState::Retracted;
    let mut parent: Vec<usize> = (0..lessons.len()).collect();
    for i in 0..lessons.len() {
        for j in i + 1..lessons.len() {
            if live(i) && live(j) && features[i].near_duplicate(&features[j]) {
                let (a, b) = (root(&parent, i), root(&parent, j));
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let roots: Vec<usize> = (0..lessons.len()).map(|i| root(&parent, i)).collect();
    let mut sizes = vec![0_usize; lessons.len()];
    let mut smallest: Vec<Option<&LessonId>> = vec![None; lessons.len()];
    for (lesson, &group) in lessons.iter().zip(&roots) {
        sizes[group] += 1;
        if smallest[group].is_none_or(|id| lesson.id < *id) {
            smallest[group] = Some(&lesson.id);
        }
    }
    roots
        .iter()
        .map(|&group| {
            (sizes[group] >= 2)
                .then(|| smallest[group].cloned())
                .flatten()
        })
        .collect()
}

fn root(parent: &[usize], mut i: usize) -> usize {
    while parent[i] != i {
        assert!(parent[i] < i, "a union points to the smaller index");
        i = parent[i];
    }
    i
}

struct Features {
    words: BTreeSet<String>,
    trigrams: BTreeSet<[char; 3]>,
}

impl Features {
    fn of(text: &str) -> Self {
        let lower = text.to_lowercase();
        let ordered: Vec<&str> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let words = ordered.iter().map(|w| (*w).to_owned()).collect();
        let joined: Vec<char> = ordered.join(" ").chars().collect();
        let trigrams = joined.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
        Self { words, trigrams }
    }

    fn near_duplicate(&self, other: &Self) -> bool {
        jaccard(&self.words, &other.words) >= MIN_WORD_SIMILARITY
            && jaccard(&self.trigrams, &other.trigrams) >= MIN_TRIGRAM_SIMILARITY
    }
}

fn jaccard<T: Ord>(a: &BTreeSet<T>, b: &BTreeSet<T>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    let shared = a.intersection(b).count();
    assert!(shared <= union);
    shared as f64 / union as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::lesson::{LessonKind, LessonSource};

    fn lesson(text: &str, state: LessonState) -> Lesson {
        let at = archon_core::now();
        let (kind, source) = (LessonKind::Practice, LessonSource::Orchestrator);
        Lesson::new(LessonScope::default(), kind, text, source, state, at).unwrap()
    }

    #[test]
    fn near_duplicates_cluster_transitively_under_their_smallest_id() {
        let lessons = [
            lesson(
                "Run the migration tests before shipping a schema change",
                LessonState::Active,
            ),
            lesson("Check the web bundle before a release", LessonState::Active),
            lesson(
                "Run the migration tests before you ship a schema change",
                LessonState::Proposed,
            ),
            lesson(
                "Always run the migration tests before you ship a schema change",
                LessonState::Active,
            ),
            lesson(
                "Run the migration tests before shipping a schema change",
                LessonState::Retracted,
            ),
        ];
        let found = clusters(&lessons);
        let key = [&lessons[0], &lessons[2], &lessons[3]]
            .map(|l| l.id.clone())
            .into_iter()
            .min();
        assert_eq!(found[0], key);
        assert_eq!(found[2], key);
        assert_eq!(found[3], key);
        assert_eq!(found[1], None, "unrelated advice stays alone");
        assert_eq!(found[4], None, "a retracted lesson joins no cluster");
        assert_eq!(clusters(&lessons), found, "clustering is deterministic");
    }

    #[test]
    fn shared_vocabulary_alone_is_not_a_duplicate() {
        let a = Features::of("Run the migration tests before shipping a schema change");
        let b = Features::of("Skip the migration tests when shipping a docs change");
        assert!(!a.near_duplicate(&b));
        assert!(a.near_duplicate(&a));
        assert_eq!(jaccard::<String>(&BTreeSet::new(), &BTreeSet::new()), 0.0);
    }
}
