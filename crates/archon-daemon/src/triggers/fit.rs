//! How much held news one batch takes (ARCHITECTURE §5.4). Aether refuses a
//! trigger payload over its limit and a refused batch is dropped, so a batch is cut to fit.

use archon_aether::wire::MAX_TRIGGER_PAYLOAD_BYTES;
use archon_core::error::ArchonError;

use super::compact::Held;

/// Most encoded bytes of held events one batch is keyed from: half of what Aether takes
/// in a trigger payload, the other half being room for the batch's directives and wrapping.
pub(super) const MAX_BATCH_BYTES: usize = MAX_TRIGGER_PAYLOAD_BYTES / 2;

/// How many of `held`, oldest first, encode within `budget` bytes together. At least
/// one: an event over the budget by itself can only go alone.
pub(super) fn fitting(held: &[Held], budget: usize) -> Result<usize, ArchonError> {
    assert!(!held.is_empty(), "only held news is keyed");
    assert!(budget > 0, "a batch has room");
    let mut bytes = 0usize;
    let mut taken = 0;
    for news in held {
        // An event and the comma after it, as the payload's `events` lists them.
        let encoded = serde_json::to_vec(&news.event)?.len() + 1;
        bytes = bytes.saturating_add(encoded);
        if taken > 0 && bytes > budget {
            break;
        }
        taken += 1;
    }
    assert!((1..=held.len()).contains(&taken), "a prefix, never empty");
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use archon_core::id::QueueItemId;

    use super::super::TriggerEvent;
    use super::*;

    fn blocked(reason_bytes: usize) -> Held {
        let event = TriggerEvent::ItemBlocked {
            item: QueueItemId::new(),
            reason: "x".repeat(reason_bytes),
        };
        Held::new(event, archon_core::now(), false)
    }

    #[test]
    fn a_batch_takes_the_oldest_news_that_fits() {
        let held = [blocked(400), blocked(400), blocked(400)];
        let one = serde_json::to_vec(&held[0].event).unwrap().len() + 1;
        assert_eq!(fitting(&held, 3 * one).unwrap(), 3, "all of it fits");
        assert_eq!(fitting(&held, 3 * one - 1).unwrap(), 2);
        assert_eq!(fitting(&held, 2 * one).unwrap(), 2);
        assert_eq!(fitting(&held, one).unwrap(), 1);
    }

    #[test]
    fn an_event_over_the_budget_goes_alone() {
        let held = [blocked(4000), blocked(10)];
        assert_eq!(fitting(&held, 100).unwrap(), 1, "it cannot be cut");
        let held = [blocked(10), blocked(4000), blocked(10)];
        assert_eq!(fitting(&held, 100).unwrap(), 1, "the small one before it");
        assert_eq!(fitting(&held[1..], 100).unwrap(), 1, "then it, alone");
    }

    /// A repository's held watch news is bounded (`MAX_HELD_NUMBERS`), so even at
    /// its cap, with the longest numbers and logins, it leaves a batch room to spare.
    #[test]
    fn a_repositorys_watch_news_at_its_cap_fits_a_batch() {
        use crate::github_sync::watch::{MAX_CHANGES, WatchedChange};
        use crate::github_sync::watch_fold::MAX_HELD_NUMBERS;
        let reviewers = MAX_HELD_NUMBERS / 2 / MAX_CHANGES;
        let change = |number: u64| WatchedChange {
            number,
            change: "review_requested".into(),
            also: vec!["new_head".into(), "new_comment".into(), "new_review".into()],
            title: "t".repeat(201),
            author: Some("a".repeat(39)),
            url: format!("https://github.com/{}/pull/{number}", "o".repeat(140)),
            reviewers: (0..reviewers).map(|r| format!("{r:039}")).collect(),
        };
        let numbers: Vec<u64> = (0..MAX_HELD_NUMBERS as u64 / 2)
            .map(|n| u64::MAX - n)
            .collect();
        let event = TriggerEvent::GithubWatch {
            repo: "r".repeat(100),
            repository: "o".repeat(140),
            changes: (1..=MAX_CHANGES as u64).map(change).collect(),
            rest: [("seen_pull_request".to_owned(), numbers)].into(),
            truncated: true,
            note: Some(crate::github_sync::watch_fold::TRUNCATED_NOTE.to_owned()),
            operator: false,
            trust: crate::github_sync::watch::WATCH_TRUST.to_owned(),
        };
        let held = [Held::new(event, archon_core::now(), false)];
        let encoded = serde_json::to_vec(&held[0].event).unwrap().len();
        assert!(encoded > 100_000, "a watch at its cap: {encoded} bytes");
        assert!(encoded * 2 < MAX_BATCH_BYTES, "{encoded} bytes of news");
    }
}
