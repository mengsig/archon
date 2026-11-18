//! Held watch news folded by repository (A18). Watch news is pinned through
//! compaction, so a walk's news joins its repository's news still held: one bounded event each.

use std::collections::{BTreeMap, BTreeSet};

use super::watch::{MAX_CHANGES, WatchedChange, by_kind};
use crate::triggers::TriggerEvent;

/// What a repository's held watch news names at most: the pull request numbers of
/// `rest`, counted once under each kind, and the reviewers of `changes` together.
pub(crate) const MAX_HELD_NUMBERS: usize = 10_000;

/// A watch trigger's `note` when its `rest` or its reviewers were cut.
pub(crate) const TRUNCATED_NOTE: &str = "More pull requests or requested reviewers changed than \
    this trigger names: list the repository's open pull requests for the rest.";

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
        truncated,
        note,
        operator,
        ..
    } = held
    else {
        unreachable!("checked as watch news");
    };
    let TriggerEvent::GithubWatch {
        changes: newer_changes,
        rest: mut newer_rest,
        truncated: newer_truncated,
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
    let mut named = std::mem::take(rest);
    // What is held stays: a cut keeps the earliest, and leaves out the news that came last.
    let mut room = MAX_HELD_NUMBERS.saturating_sub(reviewers_in(&told) + numbers_in(&named));
    let mut cut = false;
    for change in newer_changes {
        match told.iter_mut().find(|known| known.number == change.number) {
            Some(known) => cut |= join(known, change, &mut room),
            None => {
                let (change, clipped) = with_reviewers_within(change, &mut room);
                cut |= clipped;
                told.push(change);
            }
        }
    }
    for change in &mut told {
        absorb_named(change, &mut named);
        absorb_named(change, &mut newer_rest);
    }
    let past = told.split_off(told.len().min(MAX_CHANGES));
    let reviewers = reviewers_in(&told);
    cut |= add_named(&mut named, by_kind(&past), reviewers);
    cut |= add_named(&mut named, newer_rest, reviewers);
    *truncated = *truncated || newer_truncated || cut;
    *note = truncated.then(|| TRUNCATED_NOTE.to_owned());
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

/// `known` with `newer`'s kinds, freshest text and, while there is `room`, reviewers
/// joined in; whether a reviewer was left out.
fn join(known: &mut WatchedChange, newer: WatchedChange, room: &mut usize) -> bool {
    assert_eq!(known.number, newer.number, "one pull request");
    let kinds: Vec<String> = newer.kinds().map(str::to_owned).collect();
    add_kinds(known, kinds);
    known.title = newer.title;
    known.author = newer.author;
    known.url = newer.url;
    add_reviewers(&mut known.reviewers, newer.reviewers, room)
}

/// `change` with as many of its reviewers as there is `room` for; whether one was left out.
fn with_reviewers_within(mut change: WatchedChange, room: &mut usize) -> (WatchedChange, bool) {
    let asked = std::mem::take(&mut change.reviewers);
    let cut = add_reviewers(&mut change.reviewers, asked, room);
    (change, cut)
}

/// `more` added to `known`, each reviewer once, while there is `room`; whether one was left out.
fn add_reviewers(known: &mut Vec<String>, more: Vec<String>, room: &mut usize) -> bool {
    let before = known.len() + *room;
    let mut cut = false;
    for reviewer in more {
        if known.contains(&reviewer) {
            continue;
        }
        if *room == 0 {
            cut = true;
            continue;
        }
        known.push(reviewer);
        *room -= 1;
    }
    assert_eq!(known.len() + *room, before, "each one added took its room");
    assert!(!cut || *room == 0, "a cut only without room");
    cut
}

fn reviewers_in(changes: &[WatchedChange]) -> usize {
    changes.iter().map(|change| change.reviewers.len()).sum()
}

fn numbers_in(named: &BTreeMap<String, Vec<u64>>) -> usize {
    named.values().map(Vec::len).sum()
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

/// One walk's news within [`MAX_HELD_NUMBERS`]: the reviewers of `changes`, told in
/// full, first, then `rest`'s earliest kinds' numbers; whether any was left out.
pub(crate) fn capped(
    changes: Vec<WatchedChange>,
    rest: BTreeMap<String, Vec<u64>>,
) -> (Vec<WatchedChange>, BTreeMap<String, Vec<u64>>, bool) {
    assert!(changes.len() <= MAX_CHANGES, "those told in full");
    let mut room = MAX_HELD_NUMBERS;
    let mut cut = false;
    let mut told = Vec::with_capacity(changes.len());
    for change in changes {
        let (change, clipped) = with_reviewers_within(change, &mut room);
        cut |= clipped;
        told.push(change);
    }
    let mut named = BTreeMap::new();
    cut |= add_named(&mut named, rest, MAX_HELD_NUMBERS - room);
    (told, named, cut)
}

/// `more` added to `named`, each kind's numbers sorted and once, while it and the
/// `reviewers` told beside it name fewer than [`MAX_HELD_NUMBERS`]; whether any was left out.
fn add_named(
    named: &mut BTreeMap<String, Vec<u64>>,
    more: BTreeMap<String, Vec<u64>>,
    reviewers: usize,
) -> bool {
    let cap = MAX_HELD_NUMBERS.saturating_sub(reviewers);
    let mut total = numbers_in(named);
    let mut cut = false;
    for (kind, numbers) in more {
        let listed = named.entry(kind).or_default();
        let mut known: BTreeSet<u64> = listed.iter().copied().collect();
        for number in numbers {
            if known.contains(&number) {
                continue;
            }
            if total >= cap {
                cut = true;
                continue;
            }
            known.insert(number);
            total += 1;
        }
        *listed = known.into_iter().collect();
    }
    named.retain(|_, numbers| !numbers.is_empty());
    assert_eq!(numbers_in(named), total, "counted as kept");
    assert!(!cut || total >= cap, "a cut only at the cap");
    cut
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
            truncated: false,
            note: None,
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

    #[test]
    fn held_rest_is_cut_at_its_cap_keeping_the_earliest_and_says_so() {
        let earliest: Vec<u64> = (1..=MAX_HELD_NUMBERS as u64 - 1).collect();
        let mut held = news(vec![], &[("new_comment", &earliest)]);
        let later = [MAX_HELD_NUMBERS as u64 + 5, MAX_HELD_NUMBERS as u64 + 6];
        fold(&mut held, news(vec![], &[("new_review", &later)]));
        let TriggerEvent::GithubWatch {
            rest,
            truncated,
            note,
            ..
        } = &held
        else {
            panic!("watch news");
        };
        assert!(*truncated, "a cut is told");
        assert_eq!(note.as_deref(), Some(TRUNCATED_NOTE));
        assert_eq!(rest["new_comment"], earliest, "the earliest are kept");
        assert_eq!(rest["new_review"], [later[0]]);
        let total: usize = rest.values().map(Vec::len).sum();
        assert_eq!(total, MAX_HELD_NUMBERS);

        fold(&mut held, news(vec![change(3, &["new_head"])], &[]));
        let TriggerEvent::GithubWatch { truncated, .. } = &held else {
            panic!("watch news");
        };
        assert!(*truncated, "a held cut stays told");
    }

    fn asked(number: u64, reviewers: &[&str]) -> WatchedChange {
        WatchedChange {
            reviewers: reviewers.iter().map(|r| (*r).to_owned()).collect(),
            ..change(number, &["review_requested"])
        }
    }

    /// Reviewers are joined walk after walk, so they count toward the cap as
    /// `rest`'s numbers do: held news stays bounded however many are asked.
    #[test]
    fn held_reviewers_count_toward_the_cap_and_a_cut_says_so() {
        let numbers: Vec<u64> = (100..100 + MAX_HELD_NUMBERS as u64 - 2).collect();
        let mut held = news(vec![asked(1, &["a"])], &[("new_comment", &numbers)]);
        let newer = news(vec![asked(1, &["a", "b", "c"]), asked(2, &["d"])], &[]);
        fold(&mut held, newer);
        let TriggerEvent::GithubWatch {
            changes,
            rest,
            truncated,
            note,
            ..
        } = &held
        else {
            panic!("watch news");
        };
        assert_eq!(changes[0].reviewers, ["a", "b"], "the earliest are kept");
        assert_eq!((changes[1].number, changes[1].reviewers.len()), (2, 0));
        assert_eq!(reviewers_in(changes) + numbers_in(rest), MAX_HELD_NUMBERS);
        assert!(*truncated, "a cut reviewer is told");
        assert_eq!(note.as_deref(), Some(TRUNCATED_NOTE));
    }

    #[test]
    fn one_walks_news_is_cut_at_the_cap_reviewers_first() {
        let numbers: Vec<u64> = (100..100 + MAX_HELD_NUMBERS as u64).collect();
        let rest = BTreeMap::from([("new_comment".to_owned(), numbers)]);
        let (told, named, cut) = capped(vec![asked(1, &["a", "b"])], rest.clone());
        assert_eq!(told[0].reviewers, ["a", "b"]);
        assert_eq!(named["new_comment"].len(), MAX_HELD_NUMBERS - 2);
        assert!(cut, "two numbers were left out");
        let (_, named, cut) = capped(vec![change(1, &["new_head"])], rest);
        assert_eq!(named["new_comment"].len(), MAX_HELD_NUMBERS);
        assert!(!cut, "at the cap, nothing is left out");
    }
}
