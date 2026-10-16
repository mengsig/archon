//! What the store remembers of each repository's integration branch, in
//! `kv` by repository id, and how `status` shows it.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::integration::{IntegrationHealth, IntegrationStatus, short};
use archon_queue::store::Store;
use serde::{Deserialize, Serialize};

/// `kv`: the [`BranchState`] of one repository, by id.
const KEY_PREFIX: &str = "integration_";
/// Characters of a problem kept.
const MAX_PROBLEM_CHARS: usize = 400;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BranchState {
    pub branch: String,
    /// The head last read.
    #[serde(default)]
    pub head: Option<String>,
    /// When `head` was first read: the branch stood still since.
    #[serde(default)]
    pub head_since: Option<Timestamp>,
    #[serde(default)]
    pub last_green: Option<String>,
    #[serde(default)]
    pub merges_since_green: u32,
    /// The latest head verified, and how it fared.
    #[serde(default)]
    pub verdict: Option<Verdict>,
    /// The commit a full check runs on now.
    #[serde(default)]
    pub verifying: Option<String>,
    #[serde(default)]
    pub culprit: Option<Culprit>,
    /// The latest fix item queued, and the red head it was queued for.
    #[serde(default)]
    pub fix: Option<Fix>,
    #[serde(default)]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Verdict {
    pub commit: String,
    pub green: bool,
    pub at: Timestamp,
    /// `checks.full` or `GitHub checks`.
    pub via: String,
    /// A red verdict's failing output, or the failing checks' names.
    #[serde(default)]
    pub excerpt: String,
}

/// The first merge a bisection found red.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Culprit {
    pub commit: String,
    pub subject: String,
}

impl Culprit {
    pub fn line(&self) -> String {
        format!("{} {}", short(&self.commit), self.subject)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Fix {
    pub item: QueueItemId,
    pub head: String,
}

impl BranchState {
    pub fn new(branch: &str) -> Self {
        assert!(!branch.is_empty(), "a watched branch has a name");
        Self {
            branch: branch.to_owned(),
            ..Self::default()
        }
    }

    /// Note the head read at `now`; a head that moved restarts the idle clock.
    pub fn observe(&mut self, head: &str, now: Timestamp) {
        if self.head.as_deref() != Some(head) {
            self.head = Some(head.to_owned());
            self.head_since = Some(now);
        }
        self.problem = None;
        assert!(self.head_since.is_some(), "a read head has a time");
    }

    pub fn set_problem(&mut self, problem: &str) {
        assert!(!problem.trim().is_empty(), "a problem says something");
        self.problem = Some(problem.chars().take(MAX_PROBLEM_CHARS).collect());
    }

    /// The head's verdict, if it was verified.
    pub fn verdict_on_head(&self) -> Option<&Verdict> {
        let verdict = self.verdict.as_ref()?;
        (Some(&verdict.commit) == self.head.as_ref()).then_some(verdict)
    }

    pub fn health(&self) -> IntegrationHealth {
        if self.verifying.is_some() {
            return IntegrationHealth::Verifying;
        }
        match (self.verdict_on_head(), &self.head) {
            (Some(verdict), _) if verdict.green => IntegrationHealth::Green,
            (Some(_), _) => IntegrationHealth::Red,
            (None, Some(_)) if self.problem.is_none() => IntegrationHealth::Pending,
            _ => IntegrationHealth::Unknown,
        }
    }

    pub fn status(&self, repo: &str) -> IntegrationStatus {
        IntegrationStatus {
            repo: repo.to_owned(),
            branch: self.branch.clone(),
            health: self.health(),
            head: self.head.clone(),
            last_green: self.last_green.clone(),
            merges_since_green: self.merges_since_green,
            culprit: self.culprit.as_ref().map(Culprit::line),
            fix_item: self.fix.as_ref().map(|fix| fix.item.clone()),
            problem: self.problem.clone(),
            verified_at: self.verdict.as_ref().map(|verdict| verdict.at),
        }
    }
}

fn key(repo: &RepoId) -> String {
    format!("{KEY_PREFIX}{repo}")
}

/// What the store remembers of `repo`'s integration branch, if anything.
pub(crate) fn load(store: &Store, repo: &RepoId) -> Result<Option<BranchState>, ArchonError> {
    let key = key(repo);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(None);
    };
    let state: BranchState = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("kv {key} = {raw:?}: {e}")))?;
    assert!(!state.branch.is_empty(), "a saved state names its branch");
    Ok(Some(state))
}

pub(crate) fn save(
    store: &mut Store,
    repo: &RepoId,
    state: &BranchState,
) -> Result<(), ArchonError> {
    assert!(!state.branch.is_empty(), "a saved state names its branch");
    assert!(
        state.head.is_some() || state.verdict.is_none(),
        "a verdict is of a head that was read"
    );
    store.kv_set(&key(repo), &serde_json::to_string(state)?)
}

pub(crate) fn forget(store: &mut Store, repo: &RepoId) -> Result<(), ArchonError> {
    store.kv_delete(&key(repo))
}

/// Every remembered state, by repository id.
pub(crate) fn all(store: &Store) -> Result<Vec<(RepoId, BranchState)>, ArchonError> {
    let mut states = Vec::new();
    for (key, _) in store.kv_prefix(KEY_PREFIX)? {
        let id = key.strip_prefix(KEY_PREFIX).expect("ranged by prefix");
        let repo = RepoId::parse(id).map_err(|e| ArchonError::Store(format!("kv {key}: {e}")))?;
        if let Some(state) = load(store, &repo)? {
            states.push((repo, state));
        }
    }
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(commit: &str, green: bool) -> Option<Verdict> {
        Some(Verdict {
            commit: commit.into(),
            green,
            at: archon_core::now(),
            via: "checks.full".into(),
            excerpt: String::new(),
        })
    }

    #[test]
    fn health_follows_the_verdict_on_the_head_only() {
        let mut state = BranchState::new("next");
        assert_eq!(state.health(), IntegrationHealth::Unknown);
        state.observe("a", archon_core::now());
        assert_eq!(state.health(), IntegrationHealth::Pending);
        state.verdict = verdict("a", false);
        assert_eq!(state.health(), IntegrationHealth::Red);
        state.verifying = Some("a".into());
        assert_eq!(state.health(), IntegrationHealth::Verifying);
        state.verifying = None;
        let since = state.head_since;
        state.observe("b", archon_core::now());
        assert_ne!(state.head_since, since, "a moved head restarts the clock");
        assert_eq!(
            state.health(),
            IntegrationHealth::Pending,
            "b is unverified"
        );
        state.verdict = verdict("b", true);
        assert_eq!(state.health(), IntegrationHealth::Green);
        state.set_problem(&"x".repeat(1_000));
        assert_eq!(state.problem.as_ref().unwrap().len(), MAX_PROBLEM_CHARS);
    }
}
