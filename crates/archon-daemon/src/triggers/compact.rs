//! Compaction of held news (ARCHITECTURE §5.4), as it is keyed into a batch
//! and as it is pushed past [`MAX_PENDING`]. A backlog that is long, old, or
//! inherited by a fresh orchestrator keeps what still needs action; one
//! `news_digest` counts the rest by type.

use std::collections::{BTreeMap, HashMap, HashSet};

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{QuestionId, QueueItemId, ReportId};
use archon_core::queue::QueueState;
use archon_queue::Store;
use chrono::TimeDelta;
use serde::{Deserialize, Serialize};

use super::TriggerEvent;

/// Most events one compacted batch carries besides the digest; pinned
/// events are kept beyond it.
pub const MAX_BATCH_EVENTS: usize = 10;
/// News held longer than this is history, unless it is pinned.
pub const MAX_NEWS_AGE: TimeDelta = TimeDelta::hours(1);
/// Most events held behind a batch in flight before a push compacts them, so
/// a stuck batch cannot grow the outbox without bound. Well above
/// `MAX_BATCH_EVENTS`, so one compaction serves many pushes.
pub const MAX_PENDING: usize = 4 * MAX_BATCH_EVENTS;

/// One pending event and when it was pushed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Held {
    #[serde(flatten)]
    pub event: TriggerEvent,
    /// `None` for a digest, and for news saved before push times were kept;
    /// never too old.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_at: Option<Timestamp>,
    /// Routine news: it waits for the orchestrator's next turn instead of
    /// waking it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet: bool,
}

impl Held {
    fn is_old(&self, now: Timestamp) -> bool {
        self.pushed_at.is_some_and(|at| now - at > MAX_NEWS_AGE)
    }
}

/// What the store says now about the items and reports held news names.
#[derive(Debug, Default)]
pub(super) struct Facts {
    /// Current state by item; an item missing here was pruned.
    pub items: HashMap<QueueItemId, QueueState>,
    /// What each item's last run cost by now, settlements included.
    pub costs: HashMap<QueueItemId, f64>,
    /// Reports whose Lavish session is still open.
    pub open_reports: HashSet<ReportId>,
    /// Gate questions the operator has not answered yet.
    pub open_gate_questions: HashSet<QuestionId>,
}

impl Facts {
    pub fn gather(store: &mut Store, held: &[Held]) -> Result<Self, ArchonError> {
        let mut facts = Self::default();
        for Held { event, .. } in held {
            match event {
                TriggerEvent::ItemFinished { item, .. }
                | TriggerEvent::ItemBlocked { item, .. } => {
                    facts.learn_item(store, item)?;
                }
                TriggerEvent::LavishFeedback { report, .. }
                    if crate::reports::is_open(store, report)? =>
                {
                    facts.open_reports.insert(report.clone());
                }
                TriggerEvent::GateOpened { question, .. } if is_open(store, question)? => {
                    facts.open_gate_questions.insert(question.clone());
                }
                _ => {}
            }
        }
        Ok(facts)
    }

    /// `event` telling the cost its item's run settled at since it finished:
    /// a provider bills the last turns shortly after the run ends.
    pub fn with_settled_cost(&self, mut event: TriggerEvent) -> TriggerEvent {
        if let TriggerEvent::ItemFinished { item, cost_usd, .. } = &mut event
            && let Some(settled) = self.costs.get(item)
        {
            assert!(settled.is_finite() && *settled >= 0.0, "a recorded cost");
            *cost_usd = *settled;
        }
        event
    }

    fn learn_item(&mut self, store: &Store, item: &QueueItemId) -> Result<(), ArchonError> {
        if self.items.contains_key(item) {
            return Ok(());
        }
        match store.item(item) {
            Ok(found) => {
                assert_eq!(&found.id, item, "the store returns the item asked for");
                if let Some(run) = store.runs_for_item(item)?.last() {
                    let cost = match store.outcome(&run.id)? {
                        Some(outcome) => outcome.cost_usd,
                        None => store.spend_for_run(&run.id, true)?,
                    };
                    self.costs.insert(item.clone(), cost);
                }
                self.items.insert(found.id, found.state);
                Ok(())
            }
            Err(ArchonError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Whether `question` still waits for its answer; a pruned one does not.
fn is_open(store: &Store, question: &QuestionId) -> Result<bool, ArchonError> {
    match store.question(question) {
        Ok(found) => {
            assert_eq!(&found.id, question, "the store returns the question asked");
            Ok(found.is_open())
        }
        Err(ArchonError::NotFound(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// How compaction treats one held event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relevance {
    /// Needs action whatever its age: always kept.
    Pinned,
    /// Kept while recent and among the newest [`MAX_BATCH_EVENTS`].
    Current,
    /// Superseded or settled since it was pushed: never kept.
    Stale,
}

/// Whether `held` is too long or too old to replay in full.
pub(super) fn needs_compaction(held: &[Held], now: Timestamp) -> bool {
    held.len() > MAX_BATCH_EVENTS || held.iter().any(|news| news.is_old(now))
}

/// The events of `held` still worth delivering, oldest first, behind one
/// `news_digest` when any were left out. An earlier digest in `held` is
/// folded into that one, so compacting twice never loses a count.
pub(super) fn compact(held: Vec<Held>, facts: &Facts, now: Timestamp) -> Vec<Held> {
    assert!(!held.is_empty(), "only held news is compacted");
    // A digest of news that would have woken the orchestrator wakes it too.
    let quiet = held.iter().all(|news| news.quiet);
    let relevance = classify(&held, facts, now);
    assert_eq!(relevance.len(), held.len(), "one verdict per event");
    let pinned = relevance
        .iter()
        .filter(|r| **r == Relevance::Pinned)
        .count();
    let mut slots = MAX_BATCH_EVENTS.saturating_sub(pinned);
    let mut keep = vec![false; held.len()];
    for (index, verdict) in relevance.iter().enumerate().rev() {
        keep[index] = match verdict {
            Relevance::Pinned => true,
            Relevance::Current if slots > 0 => {
                slots -= 1;
                true
            }
            Relevance::Current | Relevance::Stale => false,
        };
    }
    let (dropped, kept) = split_kept(held, &keep);
    if dropped.is_empty() {
        return kept;
    }
    let mut events = vec![Held {
        event: digest(dropped),
        pushed_at: None,
        quiet,
    }];
    events.extend(kept);
    events
}

/// The events of `held` marked in `keep`, and a count by type of the rest,
/// with the counts of any digest among them added in.
fn split_kept(held: Vec<Held>, keep: &[bool]) -> (BTreeMap<String, u32>, Vec<Held>) {
    assert_eq!(held.len(), keep.len(), "one mark per event");
    let mut dropped: BTreeMap<String, u32> = BTreeMap::new();
    let mut kept = Vec::new();
    for (news, keep) in held.into_iter().zip(keep) {
        if *keep {
            kept.push(news);
            continue;
        }
        match news.event {
            TriggerEvent::NewsDigest {
                dropped: earlier, ..
            } => {
                for (kind, count) in earlier {
                    *dropped.entry(kind).or_default() += count;
                }
            }
            event => *dropped.entry(event.kind().to_owned()).or_default() += 1,
        }
    }
    assert!(
        kept.iter()
            .all(|news| !matches!(news.event, TriggerEvent::NewsDigest { .. })),
        "a digest is always folded into the new one"
    );
    (dropped, kept)
}

/// Judge each event of `held`, newest first so an item's later outcome
/// supersedes its earlier ones.
fn classify(held: &[Held], facts: &Facts, now: Timestamp) -> Vec<Relevance> {
    let mut finished_later = HashSet::new();
    let mut relevance: Vec<Relevance> = held
        .iter()
        .rev()
        .map(
            |news| match relevance_of(&news.event, facts, &mut finished_later) {
                Relevance::Current if news.is_old(now) => Relevance::Stale,
                verdict => verdict,
            },
        )
        .collect();
    relevance.reverse();
    relevance
}

/// A `gate_opened` is pinned while its question is open and stale once it
/// is answered or withdrawn; a `chat_answer` is the reply the operator awaits.
/// An `item_finished` stays current while it is the item's newest and the
/// item still sits in the state it reports; a retry supersedes it. One
/// asked for on GitHub is pinned then, as its thread awaits the outcome. An
/// `item_blocked` stays current while the item is still blocked. An earlier
/// digest is folded into the new one. A `mirror` change is rare and says what
/// the orchestrator reads is stale, so it is always kept.
fn relevance_of(
    event: &TriggerEvent,
    facts: &Facts,
    finished_later: &mut HashSet<QueueItemId>,
) -> Relevance {
    match event {
        TriggerEvent::OperatorAnswer { .. }
        | TriggerEvent::AgentMessage { .. }
        | TriggerEvent::ChatAnswer { .. }
        | TriggerEvent::Mirror { .. } => Relevance::Pinned,
        TriggerEvent::GateOpened { question, .. } => {
            match facts.open_gate_questions.contains(question) {
                true => Relevance::Pinned,
                false => Relevance::Stale,
            }
        }
        TriggerEvent::NewsDigest { .. } => Relevance::Stale,
        TriggerEvent::LavishFeedback { report, .. } if facts.open_reports.contains(report) => {
            Relevance::Pinned
        }
        TriggerEvent::ItemFinished {
            item,
            state,
            origin,
            ..
        } => {
            let newest = finished_later.insert(item.clone());
            let settled = facts
                .items
                .get(item)
                .is_some_and(|now| now.is_terminal() && now.kind() == state);
            match (newest && settled, origin) {
                (true, Some(_)) => Relevance::Pinned,
                (true, None) => Relevance::Current,
                (false, _) => Relevance::Stale,
            }
        }
        TriggerEvent::ItemBlocked { item, .. } => match facts.items.get(item) {
            Some(QueueState::Blocked { .. }) => Relevance::Current,
            _ => Relevance::Stale,
        },
        _ => Relevance::Current,
    }
}

fn digest(dropped: BTreeMap<String, u32>) -> TriggerEvent {
    let total: u32 = dropped.values().sum();
    assert!(total > 0, "a digest stands for dropped news");
    let note = format!(
        "{total} older or no longer actionable news events were left out. \
         Call list_queue or get_item for the current state of the queue, \
         or run `archon log` for the full history."
    );
    TriggerEvent::NewsDigest { dropped, note }
}

#[cfg(test)]
mod tests {
    use archon_core::github::GithubThread;
    use archon_core::id::QuestionId;

    use super::*;

    fn at(event: TriggerEvent, pushed_at: Timestamp) -> Held {
        Held {
            event,
            pushed_at: Some(pushed_at),
            quiet: false,
        }
    }

    fn budget(scope: &str) -> TriggerEvent {
        TriggerEvent::Budget {
            scope: scope.into(),
            spent_usd: 51.0,
            limit_usd: 50.0,
        }
    }

    fn finished(item: &QueueItemId, state: &str) -> TriggerEvent {
        TriggerEvent::ItemFinished {
            item: item.clone(),
            title: "t".into(),
            state: state.into(),
            summary: "s".into(),
            pr_url: None,
            cost_usd: 1.0,
            lesson_hint: None,
            origin: None,
        }
    }

    fn blocked(item: &QueueItemId) -> TriggerEvent {
        TriggerEvent::ItemBlocked {
            item: item.clone(),
            reason: "dependency failed".into(),
        }
    }

    fn answer() -> TriggerEvent {
        TriggerEvent::OperatorAnswer {
            question: QuestionId::new(),
            text: "yes".into(),
        }
    }

    fn feedback(report: &ReportId) -> TriggerEvent {
        TriggerEvent::LavishFeedback {
            report: report.clone(),
            item: None,
            text: "session:\n  status: feedback\n".into(),
            prompts: vec!["Make it blue".into()],
        }
    }

    fn failed() -> QueueState {
        QueueState::Failed {
            run: None,
            error: "boom".into(),
        }
    }

    fn compacted(held: Vec<Held>, facts: &Facts, now: Timestamp) -> Vec<TriggerEvent> {
        let held = compact(held, facts, now);
        held.into_iter().map(|news| news.event).collect()
    }

    fn dropped(events: &[TriggerEvent]) -> BTreeMap<String, u32> {
        let TriggerEvent::NewsDigest { dropped, note } = &events[0] else {
            panic!("no digest leads {events:?}");
        };
        assert!(note.contains("list_queue") && note.contains("archon log"));
        dropped.clone()
    }

    #[test]
    fn short_recent_news_needs_no_compaction() {
        let now = archon_core::now();
        let held: Vec<Held> = (0..MAX_BATCH_EVENTS)
            .map(|n| at(budget(&n.to_string()), now))
            .collect();
        assert!(!needs_compaction(&held, now));
        let legacy = Held {
            event: budget("old"),
            pushed_at: None,
            quiet: false,
        };
        assert!(!needs_compaction(&[legacy], now), "no push time is not old");
        let old = at(budget("old"), now - MAX_NEWS_AGE - TimeDelta::seconds(1));
        assert!(needs_compaction(&[old], now));
        let mut long = held;
        long.push(at(budget("one more"), now));
        assert!(needs_compaction(&long, now));
    }

    #[test]
    fn a_long_backlog_keeps_the_newest_events_and_counts_the_rest_by_type() {
        let now = archon_core::now();
        let mut held = vec![at(
            TriggerEvent::PolicyViolation {
                agent: "agt_1".into(),
                model: "m".into(),
                action: "cancelled".into(),
            },
            now,
        )];
        held.extend((0..14).map(|n| at(budget(&n.to_string()), now)));
        let events = compacted(held, &Facts::default(), now);
        assert_eq!(
            events.len(),
            MAX_BATCH_EVENTS + 1,
            "the cap plus the digest"
        );
        let counts = BTreeMap::from([("budget".to_owned(), 4), ("policy_violation".to_owned(), 1)]);
        assert_eq!(dropped(&events), counts);
        let scopes: Vec<&str> = events[1..]
            .iter()
            .map(|event| match event {
                TriggerEvent::Budget { scope, .. } => scope.as_str(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            scopes,
            ["4", "5", "6", "7", "8", "9", "10", "11", "12", "13"]
        );
    }

    #[test]
    fn answers_and_live_feedback_are_kept_however_old_or_many() {
        let now = archon_core::now();
        let long_ago = now - TimeDelta::hours(15);
        let (open, ended) = (ReportId::new(), ReportId::new());
        let facts = Facts {
            open_reports: HashSet::from([open.clone()]),
            ..Facts::default()
        };
        let mut held: Vec<Held> = (0..MAX_BATCH_EVENTS)
            .map(|_| at(answer(), long_ago))
            .collect();
        held.push(at(feedback(&open), long_ago));
        held.push(at(feedback(&ended), long_ago));
        held.push(at(budget("daily"), now));
        let events = compacted(held, &facts, now);
        let counts = BTreeMap::from([("budget".to_owned(), 1), ("lavish_feedback".to_owned(), 1)]);
        assert_eq!(dropped(&events), counts, "no slot is left for the rest");
        let answers = events
            .iter()
            .filter(|e| matches!(e, TriggerEvent::OperatorAnswer { .. }));
        assert_eq!(answers.count(), MAX_BATCH_EVENTS);
        assert!(events.contains(&feedback(&open)));
        assert!(!events.contains(&feedback(&ended)));
    }

    #[test]
    fn a_gate_is_news_only_while_it_waits_and_a_chat_outcome_always() {
        let now = archon_core::now();
        let long_ago = now - TimeDelta::hours(15);
        let item = QueueItemId::new();
        let gate = |question: &QuestionId| TriggerEvent::GateOpened {
            question: question.clone(),
            number: 3,
            item: item.clone(),
            title: "t".into(),
            subject: archon_core::run::GateSubject::Proposal,
            asks: "a".into(),
            summary: "s".into(),
            page: "p".into(),
            reply: "r".into(),
        };
        let refused = TriggerEvent::ChatAnswer {
            number: None,
            applied: false,
            text: "usage".into(),
        };
        let (waiting, decided) = (QuestionId::new(), QuestionId::new());
        let facts = Facts {
            open_gate_questions: HashSet::from([waiting.clone()]),
            ..Facts::default()
        };
        let held = vec![
            at(gate(&waiting), long_ago),
            at(gate(&decided), now),
            at(refused.clone(), long_ago),
        ];
        let events = compacted(held, &facts, now);
        let counts = BTreeMap::from([("gate_opened".to_owned(), 1)]);
        assert_eq!(dropped(&events), counts, "a decided gate is stale");
        assert_eq!(events[1..], [gate(&waiting), refused]);
    }

    #[test]
    fn an_outcome_its_github_thread_awaits_is_kept_until_superseded() {
        let now = archon_core::now();
        let long_ago = now - TimeDelta::hours(15);
        let [asked, retried] = std::array::from_fn(|_| QueueItemId::new());
        let facts = Facts {
            items: HashMap::from([
                (asked.clone(), failed()),
                (retried.clone(), QueueState::Queued),
            ]),
            ..Facts::default()
        };
        let origin: GithubThread =
            serde_json::from_value(serde_json::json!({"repository": "o/r", "number": 3})).unwrap();
        let from_github = |item: &QueueItemId| {
            let mut event = finished(item, "failed");
            if let TriggerEvent::ItemFinished { origin: slot, .. } = &mut event {
                *slot = Some(origin.clone());
            }
            event
        };
        let held = vec![
            at(from_github(&asked), long_ago),
            at(from_github(&retried), long_ago),
            at(budget("daily"), long_ago),
        ];
        let events = compacted(held, &facts, now);
        let counts = BTreeMap::from([("budget".to_owned(), 1), ("item_finished".to_owned(), 1)]);
        assert_eq!(dropped(&events), counts, "a retry still supersedes it");
        assert_eq!(events[1..], [from_github(&asked)]);
    }

    #[test]
    fn superseded_outcomes_and_settled_blocks_are_dropped() {
        let now = archon_core::now();
        let [retried, twice, settled, still_blocked, gone] =
            std::array::from_fn(|_| QueueItemId::new());
        let facts = Facts {
            items: HashMap::from([
                (retried.clone(), QueueState::Queued),
                (twice.clone(), failed()),
                (settled.clone(), QueueState::Cancelled { reason: None }),
                (
                    still_blocked.clone(),
                    QueueState::Blocked {
                        reason: "dep".into(),
                    },
                ),
            ]),
            ..Facts::default()
        };
        let held = vec![
            at(finished(&retried, "failed"), now),
            at(finished(&twice, "failed"), now),
            at(blocked(&settled), now),
            at(blocked(&still_blocked), now),
            at(finished(&gone, "succeeded"), now),
            at(finished(&twice, "failed"), now),
        ];
        let events = compacted(held, &facts, now);
        let counts = BTreeMap::from([
            ("item_blocked".to_owned(), 1),
            ("item_finished".to_owned(), 3),
        ]);
        assert_eq!(dropped(&events), counts);
        assert_eq!(
            &events[1..],
            [blocked(&still_blocked), finished(&twice, "failed")]
        );
    }

    #[test]
    fn an_outcome_the_item_no_longer_shows_is_superseded() {
        let now = archon_core::now();
        let item = QueueItemId::new();
        let facts = Facts {
            items: HashMap::from([(item.clone(), failed())]),
            ..Facts::default()
        };
        let events = compacted(vec![at(finished(&item, "succeeded"), now)], &facts, now);
        assert_eq!(events.len(), 1, "only the digest is left");
        assert_eq!(
            dropped(&events),
            BTreeMap::from([("item_finished".to_owned(), 1)])
        );
    }

    #[test]
    fn compacting_again_folds_the_earlier_digest_into_one() {
        let now = archon_core::now();
        let mut held: Vec<Held> = (0..MAX_BATCH_EVENTS + 3)
            .map(|n| at(budget(&n.to_string()), now))
            .collect();
        held = compact(held, &Facts::default(), now);
        assert_eq!(held[0].pushed_at, None, "a digest is never too old");
        held.extend((0..2).map(|_| at(answer(), now)));
        let events = compacted(held, &Facts::default(), now);
        let digests = events
            .iter()
            .filter(|e| matches!(e, TriggerEvent::NewsDigest { .. }));
        assert_eq!(digests.count(), 1, "{events:?}");
        assert_eq!(
            dropped(&events),
            BTreeMap::from([("budget".to_owned(), 5)]),
            "3 dropped first, 2 more now"
        );
        assert_eq!(events.len(), MAX_BATCH_EVENTS + 1);
    }

    #[test]
    fn a_digest_wakes_the_orchestrator_unless_all_its_news_was_quiet() {
        let now = archon_core::now();
        let routine = |n: usize| Held {
            quiet: true,
            ..at(budget(&n.to_string()), now)
        };
        let quiet: Vec<Held> = (0..MAX_BATCH_EVENTS + 2).map(routine).collect();
        let held = compact(quiet.clone(), &Facts::default(), now);
        assert!(held.iter().all(|news| news.quiet), "nothing here wakes");
        let mut mixed = vec![at(budget("first"), now)];
        mixed.extend(quiet);
        let held = compact(mixed, &Facts::default(), now);
        assert!(matches!(held[0].event, TriggerEvent::NewsDigest { .. }));
        assert!(!held[0].quiet, "the dropped waking event still wakes");
    }

    #[test]
    fn nothing_dropped_means_no_digest() {
        let now = archon_core::now();
        let held = vec![at(budget("daily"), now), at(answer(), now)];
        let events = compacted(held.clone(), &Facts::default(), now);
        let plain: Vec<TriggerEvent> = held.into_iter().map(|news| news.event).collect();
        assert_eq!(events, plain);
    }

    #[test]
    fn held_news_decodes_with_or_without_a_push_time() {
        let now = archon_core::now();
        let timed = at(budget("daily"), now);
        let raw = serde_json::to_value(&timed).unwrap();
        assert_eq!(raw["type"], "budget", "the event stays flat: {raw}");
        assert!(raw.get("pushed_at").is_some());
        let back: Held = serde_json::from_value(raw).unwrap();
        assert_eq!(back, timed);
        let legacy = serde_json::to_value(budget("daily")).unwrap();
        let back: Held = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.pushed_at, None);
        assert_eq!(back.event, budget("daily"));
        assert!(!back.quiet, "news saved before quiet news existed wakes");
        let routine = Held {
            quiet: true,
            ..at(budget("daily"), now)
        };
        let raw = serde_json::to_value(&routine).unwrap();
        assert_eq!(raw["quiet"], true);
        assert_eq!(serde_json::from_value::<Held>(raw).unwrap(), routine);
    }
}
