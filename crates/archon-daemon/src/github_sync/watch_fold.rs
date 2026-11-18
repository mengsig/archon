//! Held watch news folded by repository (A18). Watch news is pinned through
//! compaction, so a walk's news joins its repository's news still held: one bounded event each.

use std::collections::BTreeMap;

use super::watch::{MAX_CHANGES, WatchedChange, by_kind};
use crate::triggers::TriggerEvent;

/// Every kind of difference, the most telling first.
const KINDS: [&str; 6] = [
    "new_pull_request",
    "seen_pull_request",
    "new_head",
    "review_requested",
    "new_comment",
    "new_review",
];

/// Whether `held` and `event` are watch news of one repository.
pub(crate) fn same_watch(held: &TriggerEvent, event: &TriggerEvent) -> bool {
    match (held, event) {
        (
            TriggerEvent::GithubWatch {
                repo, repository, ..
            },
            TriggerEvent::GithubWatch {
                repo: other_repo,
                repository: other_repository,
                ..
            },
        ) => repo == other_repo && repository == other_repository,
        _ => false,
    }
}

/// `newer` folded into `held`: each pull request once, named with every kind
/// of difference either named, the first [`MAX_CHANGES`] in full.
pub(crate) fn fold(held: &mut TriggerEvent, newer: TriggerEvent) {
    assert!(
        same_watch(held, &newer),
        "only one repository's watch news folds"
    );
    let TriggerEvent::GithubWatch {
        changes,
        rest,
        operator,
        ..
    } = held
    else {
        unreachable!("checked as watch news");
    };
    let TriggerEvent::GithubWatch {
        changes: newer_changes,
        rest: newer_rest,
        operator: newer_operator,
        ..
    } = newer
    else {
        unreachable!("checked as watch news");
    };
    assert!(
        !*operator && !newer_operator,
        "watch news has no operator standing"
    );
    let mut told = std::mem::take(changes);
    for change in newer_changes {
        match told.iter_mut().find(|known| known.number == change.number) {
            Some(known) => join(known, change),
            None => told.push(change),
        }
    }
    let mut named = std::mem::take(rest);
    add_named(&mut named, newer_rest);
    for change in &mut told {
        absorb_named(change, &mut named);
    }
    let past = told.split_off(told.len().min(MAX_CHANGES));
    add_named(&mut named, by_kind(&past));
    assert!(
        told.len() <= MAX_CHANGES,
        "at most the first are told in full"
    );
    assert!(
        named.values().all(|numbers| !numbers.is_empty()),
        "a kind names a pull request"
    );
    *changes = told;
    *rest = named;
}

/// `known` with `newer`'s kinds, reviewers and freshest text joined in.
fn join(known: &mut WatchedChange, newer: WatchedChange) {
    assert_eq!(known.number, newer.number, "one pull request");
    let kinds: Vec<String> = newer.kinds().map(str::to_owned).collect();
    add_kinds(known, kinds);
    for reviewer in newer.reviewers {
        if !known.reviewers.contains(&reviewer) {
            known.reviewers.push(reviewer);
        }
    }
    known.title = newer.title;
    known.author = newer.author;
    known.url = newer.url;
}

/// The kinds `named` lists `change` under, moved onto `change` itself.
fn absorb_named(change: &mut WatchedChange, named: &mut BTreeMap<String, Vec<u64>>) {
    let mut kinds = Vec::new();
    for (kind, numbers) in named.iter_mut() {
        let before = numbers.len();
        numbers.retain(|number| *number != change.number);
        if numbers.len() < before {
            kinds.push(kind.clone());
        }
    }
    named.retain(|_, numbers| !numbers.is_empty());
    add_kinds(change, kinds);
}

/// `change` named with `kinds` too, its most telling kind first.
fn add_kinds(change: &mut WatchedChange, kinds: Vec<String>) {
    let mut all: Vec<String> = change.kinds().map(str::to_owned).collect();
    all.extend(kinds);
    all.sort_by_key(|kind| rank(kind));
    all.dedup();
    assert!(!all.is_empty(), "a change has a kind");
    change.change = all.remove(0);
    change.also = all;
}

fn rank(kind: &str) -> usize {
    KINDS
        .iter()
        .position(|known| *known == kind)
        .unwrap_or(KINDS.len())
}

/// `more` added to `named`, each kind's numbers sorted and once.
fn add_named(named: &mut BTreeMap<String, Vec<u64>>, more: BTreeMap<String, Vec<u64>>) {
    for (kind, numbers) in more {
        let listed = named.entry(kind).or_default();
        listed.extend(numbers);
        listed.sort_unstable();
        listed.dedup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(number: u64, kinds: &[&str]) -> WatchedChange {
        WatchedChange {
            number,
            change: kinds[0].to_owned(),
            also: kinds[1..].iter().map(|k| (*k).to_owned()).collect(),
            title: format!("t{number}"),
            author: None,
            url: format!("u{number}"),
            reviewers: vec![],
        }
    }

    fn news(changes: Vec<WatchedChange>, rest: &[(&str, &[u64])]) -> TriggerEvent {
        TriggerEvent::GithubWatch {
            repo: "r".into(),
            repository: "o/r".into(),
            changes,
            rest: rest
                .iter()
                .map(|(k, n)| ((*k).to_owned(), n.to_vec()))
                .collect(),
            operator: false,
            trust: "news only".into(),
        }
    }

    type Told = Vec<(u64, String, Vec<String>)>;

    fn told(event: &TriggerEvent) -> (Told, BTreeMap<String, Vec<u64>>) {
        let TriggerEvent::GithubWatch { changes, rest, .. } = event else {
            panic!("watch news");
        };
        let changes = changes
            .iter()
            .map(|c| (c.number, c.change.clone(), c.also.clone()))
            .collect();
        (changes, rest.clone())
    }

    #[test]
    fn a_pull_request_in_both_is_told_once_with_every_kind() {
        let mut held = news(vec![change(1, &["new_comment"])], &[("new_review", &[9])]);
        let newer = news(
            vec![change(1, &["new_head"]), change(9, &["new_comment"])],
            &[],
        );
        fold(&mut held, newer);
        let (changes, rest) = told(&held);
        assert_eq!(
            changes,
            [
                (1, "new_head".to_owned(), vec!["new_comment".to_owned()]),
                (9, "new_comment".to_owned(), vec!["new_review".to_owned()]),
            ]
        );
        assert!(
            rest.is_empty(),
            "a pull request told in full is not in rest"
        );
    }

    #[test]
    fn folded_news_past_the_full_ones_is_named_in_rest() {
        let first: Vec<WatchedChange> = (1..=MAX_CHANGES as u64)
            .map(|n| change(n, &["new_comment"]))
            .collect();
        let mut held = news(first, &[("new_review", &[50])]);
        let newer = news(
            vec![change(30, &["new_pull_request"])],
            &[("new_review", &[40])],
        );
        fold(&mut held, newer);
        let (changes, rest) = told(&held);
        assert_eq!(changes.len(), MAX_CHANGES);
        assert_eq!(rest["new_pull_request"], [30]);
        assert_eq!(rest["new_review"], [40, 50]);
    }
}
