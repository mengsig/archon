//! Routine outcomes (ARCHITECTURE §5.4): an item that finished without
//! needing the orchestrator does not wake it. Its news waits as quiet news,
//! and the next trigger lists every such item in one `items_finished` event.

use std::collections::HashSet;

use archon_core::id::QueueItemId;
use serde::{Deserialize, Serialize};

use super::TriggerEvent;
use super::compact::{Facts, Held};

/// Most items one `items_finished` event lists; the rest are counted.
pub const MAX_LISTED: usize = 20;

/// One line of an `items_finished` event: what `get_item` is asked about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinishedItem {
    pub item: QueueItemId,
    pub title: String,
    /// `QueueState::kind()`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    pub cost_usd: f64,
}

/// Split `held` into its quiet outcomes, folded into one event without those
/// the item no longer shows, and the rest in order.
pub(super) fn fold(held: Vec<Held>, facts: &Facts) -> (Option<TriggerEvent>, Vec<Held>) {
    let before = held.len();
    let mut outcomes = Vec::new();
    let mut rest = Vec::new();
    for news in held {
        match news {
            Held {
                quiet: true,
                event:
                    TriggerEvent::ItemFinished {
                        item,
                        title,
                        state,
                        pr_url,
                        cost_usd,
                        ..
                    },
                ..
            } => outcomes.push(FinishedItem {
                item,
                title,
                state,
                pr_url,
                cost_usd,
            }),
            other => rest.push(other),
        }
    }
    assert_eq!(
        outcomes.len() + rest.len(),
        before,
        "every held event is folded or kept"
    );
    outcomes.retain(|outcome| {
        let shown = facts.items.get(&outcome.item);
        shown.is_some_and(|now| now.kind() == outcome.state)
    });
    (listed(outcomes), rest)
}

/// The newest outcome of each item, oldest first, at most [`MAX_LISTED`].
fn listed(outcomes: Vec<FinishedItem>) -> Option<TriggerEvent> {
    if outcomes.is_empty() {
        return None;
    }
    let mut seen = HashSet::new();
    let mut items: Vec<FinishedItem> = outcomes
        .into_iter()
        .rev()
        .filter(|outcome| seen.insert(outcome.item.clone()))
        .collect();
    let distinct = items.len();
    items.truncate(MAX_LISTED);
    items.reverse();
    assert!(
        !items.is_empty() && items.len() <= MAX_LISTED,
        "the list is bounded and never empty"
    );
    let more = u32::try_from(distinct - items.len()).expect("held news is bounded");
    Some(TriggerEvent::ItemsFinished { items, more })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use archon_core::id::PipelineRunId;
    use archon_core::queue::QueueState;

    use super::*;

    fn finished(item: &QueueItemId, title: &str, quiet: bool) -> Held {
        Held {
            event: TriggerEvent::ItemFinished {
                item: item.clone(),
                title: title.into(),
                state: "succeeded".into(),
                summary: "a long summary the list leaves out".into(),
                pr_url: Some("https://github.com/o/r/pull/1".into()),
                cost_usd: 1.5,
                origin: None,
                lesson_hint: None,
            },
            pushed_at: Some(archon_core::now()),
            quiet,
        }
    }

    fn succeeded(items: &[&QueueItemId]) -> Facts {
        let state = || QueueState::Succeeded {
            run: PipelineRunId::new(),
            summary: "done".into(),
        };
        Facts {
            items: items.iter().map(|id| ((*id).clone(), state())).collect(),
            ..Facts::default()
        }
    }

    #[test]
    fn quiet_outcomes_become_one_short_list_and_waking_news_is_kept() {
        let [routine, loud] = std::array::from_fn(|_| QueueItemId::new());
        let held = vec![
            finished(&routine, "bump a dependency", true),
            finished(&loud, "fix the login bug", false),
        ];
        let (list, rest) = fold(held.clone(), &succeeded(&[&routine, &loud]));
        assert_eq!(rest, held[1..], "waking news is delivered in full");
        let Some(TriggerEvent::ItemsFinished { items, more }) = list else {
            panic!("no list: {list:?}");
        };
        assert_eq!(more, 0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item, routine);
        let raw = serde_json::to_string(&items[0]).unwrap();
        assert!(!raw.contains("summary"), "the list is compact: {raw}");
        assert!(raw.contains("pull/1") && raw.contains("bump a dependency"));
    }

    #[test]
    fn an_item_is_listed_once_with_its_newest_outcome() {
        let item = QueueItemId::new();
        let held = vec![
            finished(&item, "first run", true),
            finished(&item, "second run", true),
        ];
        let (list, rest) = fold(held, &succeeded(&[&item]));
        assert!(rest.is_empty());
        let Some(TriggerEvent::ItemsFinished { items, more }) = list else {
            panic!("no list");
        };
        assert_eq!((items.len(), more), (1, 0));
        assert_eq!(items[0].title, "second run");
    }

    #[test]
    fn an_outcome_superseded_since_is_left_out() {
        let [retried, pruned] = std::array::from_fn(|_| QueueItemId::new());
        let facts = Facts {
            items: HashMap::from([(retried.clone(), QueueState::Queued)]),
            ..Facts::default()
        };
        let held = vec![
            finished(&retried, "retried since", true),
            finished(&pruned, "pruned since", true),
        ];
        let (list, rest) = fold(held, &facts);
        assert_eq!(list, None, "nothing is left to tell");
        assert!(rest.is_empty());
    }

    #[test]
    fn a_long_list_keeps_the_newest_and_counts_the_rest() {
        let ids: Vec<QueueItemId> = (0..MAX_LISTED + 3).map(|_| QueueItemId::new()).collect();
        let held: Vec<Held> = ids
            .iter()
            .enumerate()
            .map(|(n, id)| finished(id, &n.to_string(), true))
            .collect();
        let facts = succeeded(&ids.iter().collect::<Vec<_>>());
        let (list, _) = fold(held, &facts);
        let Some(TriggerEvent::ItemsFinished { items, more }) = list else {
            panic!("no list");
        };
        assert_eq!((items.len(), more), (MAX_LISTED, 3));
        assert_eq!(items[0].title, "3", "the oldest three are only counted");
        assert_eq!(items.last().unwrap().title, (MAX_LISTED + 2).to_string());
    }
}
