//! Round trips for every record type the store keeps.

mod common;

use archon_core::aether_ids::AetherQuestionRef;
use archon_core::autonomy::AutonomyPatch;
use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, QuestionId, ReportId};
use archon_core::notify::{AnswerVia, FeedEntry, Notification, NotificationKind, Question};
use archon_core::protocol::{ItemQuery, RepoRef};
use archon_core::queue::{PauseState, Priority, QueueState};
use archon_core::run::{RunState, StageState, Verdict};
use archon_queue::store::{SpendRow, keys};
use common::*;

#[test]
fn feed_and_delivery_state_commit_or_roll_back_together() {
    let mut store = store();
    let entry = FeedEntry::System {
        text: "message".into(),
    };
    let invalid =
        store.append_feed_with_kv(entry.clone(), t(0), &[("outbox", "held"), ("", "bad")]);
    assert!(matches!(invalid, Err(ArchonError::Invalid(_))));
    assert!(store.feed_after(0, 10).unwrap().is_empty());
    assert_eq!(store.kv_get("outbox").unwrap(), None);
    let receipt = store
        .append_feed_with_kv(entry.clone(), t(1), &[("outbox", "held"), ("count", "1")])
        .unwrap();
    assert_eq!(receipt.seq, 1);
    assert_eq!(receipt.entry, entry);
    assert_eq!(store.kv_get("outbox").unwrap().as_deref(), Some("held"));
    assert_eq!(store.kv_get("count").unwrap().as_deref(), Some("1"));
}

#[test]
fn repos_round_trip_by_id_and_name() {
    let mut store = store();
    let alpha = repo("alpha");
    let mut beta = repo("beta");
    beta.autonomy_max = AutonomyPatch::parse_list("merge=auto").unwrap();
    beta.base_branch = None;
    store.insert_repo(&beta).unwrap();
    store.insert_repo(&alpha).unwrap();
    assert_eq!(store.repo(&RepoRef(alpha.id.to_string())).unwrap(), alpha);
    assert_eq!(store.repo(&RepoRef("beta".into())).unwrap(), beta);
    assert_eq!(store.repos().unwrap(), vec![alpha.clone(), beta]);
    let missing = store.repo(&RepoRef("gamma".into())).unwrap_err();
    assert!(matches!(missing, ArchonError::NotFound(_)), "{missing}");

    let mut renamed = alpha.clone();
    renamed.name = "alpha2".into();
    renamed.ship = archon_core::repo::ShipMode::Pr;
    store.update_repo(&renamed).unwrap();
    assert_eq!(store.repo(&RepoRef("alpha2".into())).unwrap(), renamed);
    assert!(store.repo(&RepoRef("alpha".into())).is_err());
}

#[test]
fn repo_names_and_paths_are_unique_and_valid() {
    let mut store = store();
    let alpha = repo("alpha");
    store.insert_repo(&alpha).unwrap();
    let same_name = repo("alpha");
    assert!(matches!(
        store.insert_repo(&same_name),
        Err(ArchonError::Invalid(_))
    ));
    let mut same_path = repo("other");
    same_path.path = alpha.path.clone();
    assert!(matches!(
        store.insert_repo(&same_path),
        Err(ArchonError::Invalid(_))
    ));
    assert!(matches!(
        store.insert_repo(&repo("../x")),
        Err(ArchonError::Invalid(_))
    ));
    let mut relative = repo("rel");
    relative.path = "relative/path".into();
    assert!(matches!(
        store.insert_repo(&relative),
        Err(ArchonError::Invalid(_))
    ));
    let beta = repo("beta");
    store.insert_repo(&beta).unwrap();
    let clash = archon_core::repo::Repo {
        name: "alpha".into(),
        ..beta
    };
    assert!(matches!(
        store.update_repo(&clash),
        Err(ArchonError::Invalid(_))
    ));
    assert_eq!(store.repos().unwrap().len(), 2);
}

#[test]
fn repo_removal_waits_for_unfinished_items_and_leaves_pause() {
    let (mut store, repo, items) = seeded(1);
    let pause = PauseState {
        repos: vec![repo.id.clone()],
        ..PauseState::default()
    };
    store.set_pause_state(&pause).unwrap();
    let refused = store.remove_repo(&repo.id).unwrap_err();
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused}");
    assert_eq!(store.repos().unwrap().len(), 1);

    let cancelled = archon_core::queue::QueueState::Cancelled { reason: None };
    store
        .transition_item(&items[0].id, cancelled, t(2))
        .unwrap();
    store.remove_repo(&repo.id).unwrap();
    assert!(store.repos().unwrap().is_empty());
    assert!(store.pause_state().unwrap().repos.is_empty());
    // History keeps the item and its repository id.
    assert_eq!(
        store.item(&items[0].id).unwrap().repo,
        Some(repo.id.clone())
    );
    assert!(matches!(
        store.remove_repo(&repo.id),
        Err(ArchonError::NotFound(_))
    ));
}

#[test]
fn items_round_trip_and_update_non_state_fields() {
    let (mut store, _repo, items) = seeded(2);
    let mut first = store.item(&items[0].id).unwrap();
    assert_eq!(first, items[0]);
    first.priority = Priority::Critical;
    first.tags = vec!["x".into(), "y".into()];
    first.budget_usd = None;
    first.brief = "new brief".into();
    first.dependencies = vec![items[1].id.clone()];
    first.updated_at = t(3);
    store.update_item(&first).unwrap();
    assert_eq!(store.item(&first.id).unwrap(), first);
    assert_eq!(
        store.dependents(&items[1].id).unwrap(),
        vec![first.id.clone()]
    );

    let mut moved = first.clone();
    moved.state = archon_core::queue::QueueState::Dispatching;
    let refused = store.update_item(&moved).unwrap_err();
    assert!(
        matches!(refused, ArchonError::InvalidTransition(_)),
        "{refused}"
    );
    let mut repiped = first.clone();
    repiped.pipeline = "other".into();
    assert!(matches!(
        store.update_item(&repiped),
        Err(ArchonError::Invalid(_))
    ));
    assert_eq!(store.item(&first.id).unwrap(), first);
}

#[test]
fn item_listing_filters_and_orders_newest_first() {
    let (mut store, repo, items) = seeded(3);
    let other = common::repo("beta");
    store.insert_repo(&other).unwrap();
    let mut elsewhere = item(Some(&other), "elsewhere");
    elsewhere.pipeline = "ask".into();
    elsewhere.tags = vec!["t2".into()];
    store.insert_item(&elsewhere, None).unwrap();
    store
        .transition_item(
            &items[1].id,
            archon_core::queue::QueueState::Dispatching,
            t(2),
        )
        .unwrap();

    let ids =
        |q: &ItemQuery| -> Vec<_> { store.items(q).unwrap().into_iter().map(|i| i.id).collect() };
    let all = ids(&ItemQuery::default());
    assert_eq!(
        all,
        vec![
            elsewhere.id.clone(),
            items[2].id.clone(),
            items[1].id.clone(),
            items[0].id.clone()
        ]
    );
    let by_repo = ItemQuery {
        repo: Some(RepoRef(repo.name.clone())),
        ..ItemQuery::default()
    };
    assert_eq!(ids(&by_repo).len(), 3);
    let by_state = ItemQuery {
        states: vec!["dispatching".into()],
        ..ItemQuery::default()
    };
    assert_eq!(ids(&by_state), vec![items[1].id.clone()]);
    let by_tag = ItemQuery {
        tag: Some("t2".into()),
        ..ItemQuery::default()
    };
    assert_eq!(ids(&by_tag), vec![elsewhere.id.clone()]);
    let by_pipeline = ItemQuery {
        pipeline: Some("ask".into()),
        ..ItemQuery::default()
    };
    assert_eq!(ids(&by_pipeline), vec![elsewhere.id.clone()]);
    let limited = ItemQuery {
        limit: Some(2),
        ..ItemQuery::default()
    };
    assert_eq!(ids(&limited), all[..2].to_vec());
}

#[test]
fn item_listing_refuses_bad_filters() {
    let (store, _repo, _items) = seeded(1);
    let bad_state = ItemQuery {
        states: vec!["done".into()],
        ..ItemQuery::default()
    };
    assert!(matches!(
        store.items(&bad_state),
        Err(ArchonError::Invalid(_))
    ));
    let bad_repo = ItemQuery {
        repo: Some(RepoRef("nope".into())),
        ..ItemQuery::default()
    };
    assert!(matches!(
        store.items(&bad_repo),
        Err(ArchonError::NotFound(_))
    ));
    let zero = ItemQuery {
        limit: Some(0),
        ..ItemQuery::default()
    };
    assert!(matches!(store.items(&zero), Err(ArchonError::Invalid(_))));
}

#[test]
fn item_listing_defaults_to_fifty() {
    let (store, _repo, _items) = seeded(55);
    assert_eq!(store.items(&ItemQuery::default()).unwrap().len(), 50);
    let all = ItemQuery {
        limit: Some(usize::MAX),
        ..ItemQuery::default()
    };
    assert_eq!(store.items(&all).unwrap().len(), 55);
}

#[test]
fn runs_and_stage_runs_round_trip() {
    let (mut store, _repo, items) = seeded(1);
    let mut run = start(&mut store, &items[0].id);
    assert_eq!(store.run(&run.id).unwrap(), run);
    assert_eq!(store.active_runs().unwrap(), vec![run.clone()]);

    let spawning = stage(&run, 0);
    store.upsert_stage_run(&spawning).unwrap();
    assert_eq!(store.stage_run(&spawning.id).unwrap(), spawning);
    let mut done = running_stage(&run, 0, 1);
    done.id = spawning.id.clone();
    store.upsert_stage_run(&done).unwrap();
    done.state = StageState::Completed;
    done.verdict = Some(Verdict::Pass);
    done.summary = Some("ok\nVERDICT: PASS".into());
    done.structured = Some(serde_json::json!({"verdict": "pass", "n": 1.5}));
    done.cost_usd = 0.75;
    done.finished_at = Some(t(30));
    store.upsert_stage_run(&done).unwrap();
    assert_eq!(store.stage_run(&done.id).unwrap(), done);
    assert_eq!(
        store.stage_run_by_agent(&agent(1)).unwrap(),
        Some(done.clone())
    );
    assert_eq!(store.stage_run_by_agent(&agent(2)).unwrap(), None);

    run.aether_run = Some("run_01".to_owned().try_into().unwrap());
    run.cost_usd = 0.75;
    store.update_run(&run).unwrap();
    run.state = RunState::Succeeded {
        summary: "shipped".into(),
    };
    run.finished_at = Some(t(40));
    let done = archon_core::queue::QueueState::Succeeded {
        run: run.id.clone(),
        summary: "shipped".into(),
    };
    store.finish_run(&run, done, t(40)).unwrap();
    assert_eq!(store.run(&run.id).unwrap(), run);
    assert_eq!(
        store.runs_for_item(&items[0].id).unwrap(),
        vec![run.clone()]
    );
    assert!(store.active_runs().unwrap().is_empty());
}

#[test]
fn stage_runs_are_ordered_by_start_attempt_and_slot() {
    let (mut store, _repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let mut later = stage(&run, 0);
    later.attempt = 2;
    later.started_at = t(50);
    let slot1 = stage(&run, 1);
    let slot0 = stage(&run, 0);
    let mut retry = stage(&run, 0);
    retry.attempt = 2;
    for s in [&later, &slot1, &retry, &slot0] {
        store.upsert_stage_run(s).unwrap();
    }
    let order: Vec<_> = store
        .stage_runs(&run.id)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(order, vec![slot0.id, slot1.id, retry.id, later.id]);
}

/// One read for all of an item's stage runs, in the order reading them run
/// by run gives, even when a later run's stages started earlier.
#[test]
fn an_items_stage_runs_come_run_by_run_in_one_read() {
    let (mut store, _repo, items) = seeded(2);
    let first = start(&mut store, &items[0].id);
    let mut late_slot = stage(&first, 1);
    late_slot.started_at = t(90);
    for s in [&late_slot, &stage(&first, 0)] {
        store.upsert_stage_run(s).unwrap();
    }
    let failed = QueueState::Failed {
        run: Some(first.id.clone()),
        error: "boom".into(),
    };
    settle_run(&mut store, &first, failed, t(95));
    store
        .transition_item(&items[0].id, QueueState::Queued, t(96))
        .unwrap();
    let second = start(&mut store, &items[0].id);
    let mut early = stage(&second, 0);
    early.started_at = t(1);
    let other = start(&mut store, &items[1].id);
    for s in [&early, &stage(&other, 0)] {
        store.upsert_stage_run(s).unwrap();
    }

    let runs = store.runs_for_item(&items[0].id).unwrap();
    assert_eq!(runs.len(), 2);
    let mut run_by_run = Vec::new();
    for run in &runs {
        run_by_run.extend(store.stage_runs(&run.id).unwrap());
    }
    assert_eq!(run_by_run.len(), 3);
    assert_eq!(store.stage_runs_for_item(&items[0].id).unwrap(), run_by_run);
    let missing = store.stage_runs_for_item(&archon_core::id::QueueItemId::new());
    assert!(
        matches!(missing, Err(ArchonError::NotFound(_))),
        "{missing:?}"
    );
}

fn notification(kind: NotificationKind, text: &str) -> FeedEntry {
    FeedEntry::Notification(Notification {
        id: NotificationId::new(),
        kind,
        text: text.into(),
        item: None,
        run: None,
        at: t(0),
    })
}

#[test]
fn feed_entries_round_trip_with_gapless_seq() {
    let mut store = store();
    assert_eq!(store.last_feed_seq().unwrap(), 0);
    let entries = vec![
        FeedEntry::Operator { text: "hi".into() },
        FeedEntry::Orchestrator {
            text: "hello".into(),
        },
        notification(NotificationKind::Progress, "50%"),
        FeedEntry::Answer {
            question: QuestionId::new(),
            text: "yes".into(),
            via: AnswerVia::ChatCommand,
        },
        FeedEntry::Report {
            report: ReportId::new(),
            title: "r".into(),
            location: "/tmp/r.html".into(),
            item: None,
        },
        FeedEntry::System {
            text: "aether reconnected".into(),
        },
        notification(NotificationKind::Blocker, "budget"),
    ];
    let mut appended = Vec::new();
    for (i, entry) in entries.into_iter().enumerate() {
        let item = store.append_feed(entry, t(i as i64)).unwrap();
        assert_eq!(item.seq, i as u64 + 1);
        appended.push(item);
    }
    assert_eq!(store.last_feed_seq().unwrap(), 7);
    assert_eq!(store.feed_after(0, 100).unwrap(), appended);
    assert_eq!(store.feed_after(5, 1).unwrap(), vec![appended[5].clone()]);
    assert!(store.feed_after(7, 10).unwrap().is_empty());
}

#[test]
fn feed_history_hides_progress_unless_verbose() {
    let mut store = store();
    let kinds = [
        NotificationKind::Decision,
        NotificationKind::Progress,
        NotificationKind::Outcome,
        NotificationKind::Progress,
        NotificationKind::Question,
    ];
    for (i, kind) in kinds.into_iter().enumerate() {
        store
            .append_feed(notification(kind, "n"), t(i as i64))
            .unwrap();
    }
    let seqs = |items: Vec<archon_core::notify::FeedItem>| -> Vec<u64> {
        items.into_iter().map(|i| i.seq).collect()
    };
    assert_eq!(
        seqs(store.feed_before(None, 10, false).unwrap()),
        vec![1, 3, 5]
    );
    assert_eq!(
        seqs(store.feed_before(None, 10, true).unwrap()),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(seqs(store.feed_before(None, 2, false).unwrap()), vec![3, 5]);
    assert_eq!(
        seqs(store.feed_before(Some(5), 2, true).unwrap()),
        vec![3, 4]
    );
    assert_eq!(
        seqs(store.feed_before(Some(3), 10, false).unwrap()),
        vec![1]
    );
    assert!(store.feed_before(Some(1), 10, true).unwrap().is_empty());
}

fn question(text: &str, at: i64) -> Question {
    Question {
        id: QuestionId::new(),
        number: 0,
        text: text.into(),
        options: vec!["yes".into(), "no".into()],
        item: None,
        aether: None,
        asked_at: t(at),
        answer: None,
        answered_at: None,
    }
}

#[test]
fn questions_round_trip_and_answer_once() {
    let mut store = store();
    let first = question("merge?", 1);
    let mut second = question("which model?", 2);
    second.aether = Some(AetherQuestionRef {
        agent: agent(9),
        seq: 42,
    });
    let second = store.insert_question(&second).unwrap();
    let first = store.insert_question(&first).unwrap();
    assert_eq!(
        (second.number, first.number),
        (1, 2),
        "numbered as recorded"
    );
    assert_eq!(
        store.questions(false).unwrap(),
        vec![first.clone(), second.clone()]
    );
    assert_eq!(
        store
            .question_by_aether(second.aether.as_ref().unwrap())
            .unwrap(),
        Some(second.clone())
    );

    assert_eq!(store.question(&second.id).unwrap(), second);

    let answered = store.answer_question(&first.id, "yes", t(3)).unwrap();
    assert_eq!(answered.answer.as_deref(), Some("yes"));
    assert_eq!(answered.answered_at, Some(t(3)));
    assert_eq!(store.question(&first.id).unwrap(), answered);
    assert_eq!(store.questions(true).unwrap(), vec![second.clone()]);
    let again = store.answer_question(&first.id, "no", t(4)).unwrap_err();
    assert!(
        matches!(again, ArchonError::InvalidTransition(_)),
        "{again}"
    );
    assert_eq!(store.questions(false).unwrap()[0], answered);

    let mut replay = question("which model?", 5);
    replay.aether = second.aether.clone();
    assert!(matches!(
        store.insert_question(&replay),
        Err(ArchonError::Invalid(_))
    ));
    assert!(matches!(
        store.insert_question(&first),
        Err(ArchonError::Invalid(_))
    ));
    assert!(matches!(
        store.answer_question(&QuestionId::new(), "x", t(6)),
        Err(ArchonError::NotFound(_))
    ));
    assert!(matches!(
        store.question(&QuestionId::new()),
        Err(ArchonError::NotFound(_))
    ));
    assert_eq!(store.questions(false).unwrap().len(), 2);
}

#[test]
fn question_numbers_are_permanent_and_never_reused() {
    let mut store = store();
    let gate_b = store
        .insert_question(&question("Approve gate B", 1))
        .unwrap();
    let gate_c = store
        .insert_question(&question("Approve gate C", 2))
        .unwrap();
    store.answer_question(&gate_b.id, "yes", t(3)).unwrap();
    let gate_d = store
        .insert_question(&question("Approve gate D", 4))
        .unwrap();
    assert_eq!(
        [gate_b.number, gate_c.number, gate_d.number],
        [1, 2, 3],
        "a closed question keeps its number and the next one moves on"
    );
    let open: Vec<(u64, String)> = store
        .questions(true)
        .unwrap()
        .into_iter()
        .map(|q| (q.number, q.text))
        .collect();
    assert_eq!(
        open,
        [
            (2, "Approve gate C".to_owned()),
            (3, "Approve gate D".to_owned())
        ]
    );
    let answered = store.answer_question(&gate_c.id, "no", t(5)).unwrap();
    assert_eq!(answered.number, 2, "answering keeps the number");
}

fn spend_row(
    n: u32,
    item: Option<&archon_core::queue::QueueItem>,
    cost: f64,
    est: f64,
    at: i64,
) -> SpendRow {
    SpendRow {
        agent: agent(n),
        item: item.map(|i| i.id.clone()),
        cost_usd: cost,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        estimated_cost_usd: est,
        input_tokens: 1000,
        output_tokens: 200,
        at: t(at),
    }
}

#[test]
fn spend_rows_replace_and_sum_per_item() {
    let (mut store, _repo, items) = seeded(2);
    store
        .record_spend(&spend_row(1, Some(&items[0]), 1.0, 0.5, 10))
        .unwrap();
    store
        .record_spend(&spend_row(1, Some(&items[0]), 2.0, 1.0, 20))
        .unwrap();
    store
        .record_spend(&spend_row(2, Some(&items[0]), 0.25, 0.0, 20))
        .unwrap();
    store
        .record_spend(&spend_row(3, Some(&items[1]), 4.0, 0.0, 20))
        .unwrap();
    store
        .record_spend(&spend_row(4, None, 0.0, 3.0, 20))
        .unwrap();
    assert_eq!(store.spend_for_item(&items[0].id, true).unwrap(), 3.25);
    assert_eq!(store.spend_for_item(&items[0].id, false).unwrap(), 2.25);
    assert_eq!(store.spend_totals(t(20), true).unwrap().1, 10.25);
    assert_eq!(store.spend_totals(t(20), false).unwrap().1, 6.25);

    // A replayed older total changes nothing.
    store
        .record_spend(&spend_row(1, Some(&items[0]), 1.0, 0.5, 10))
        .unwrap();
    assert_eq!(store.spend_for_item(&items[0].id, true).unwrap(), 3.25);
    let negative = spend_row(5, None, -1.0, 0.0, 20);
    assert!(matches!(
        store.record_spend(&negative),
        Err(ArchonError::Invalid(_))
    ));
    let nan = spend_row(5, None, f64::NAN, 0.0, 20);
    assert!(matches!(
        store.record_spend(&nan),
        Err(ArchonError::Invalid(_))
    ));
    let unknown = archon_core::id::QueueItemId::new();
    assert!(matches!(
        store.spend_for_item(&unknown, true),
        Err(ArchonError::NotFound(_))
    ));
}

#[test]
fn pause_state_and_kv_round_trip() {
    let (mut store, repo, _items) = seeded(0);
    assert_eq!(store.pause_state().unwrap(), PauseState::default());
    let pause = PauseState {
        global: true,
        repos: vec![repo.id.clone()],
        pipelines: vec!["quick-pr".into()],
    };
    store.set_pause_state(&pause).unwrap();
    assert_eq!(store.pause_state().unwrap(), pause);
    let unknown = PauseState {
        repos: vec![archon_core::id::RepoId::new()],
        ..PauseState::default()
    };
    assert!(matches!(
        store.set_pause_state(&unknown),
        Err(ArchonError::NotFound(_))
    ));
    let twice = PauseState {
        pipelines: vec!["a".into(), "a".into()],
        ..PauseState::default()
    };
    assert!(matches!(
        store.set_pause_state(&twice),
        Err(ArchonError::Invalid(_))
    ));
    assert_eq!(store.pause_state().unwrap(), pause);

    assert_eq!(store.kv_get(keys::CURSOR_FLEET).unwrap(), None);
    store.kv_set(keys::CURSOR_FLEET, "17").unwrap();
    store.kv_set(keys::CURSOR_FLEET, "18").unwrap();
    assert_eq!(
        store.kv_get(keys::CURSOR_FLEET).unwrap().as_deref(),
        Some("18")
    );
    assert!(matches!(
        store.kv_set("", "x"),
        Err(ArchonError::Invalid(_))
    ));
}

#[test]
fn kv_prefix_scans_and_delete_forgets() {
    let mut store = common::store();
    store.kv_set("run_node:b", "2").unwrap();
    store.kv_set("run_node:a", "1").unwrap();
    // A key that merely contains the prefix, and one where `_` must not
    // act as a wildcard.
    store.kv_set("other:run_node:c", "3").unwrap();
    store.kv_set("runXnode:d", "4").unwrap();
    let scanned = store.kv_prefix("run_node:").unwrap();
    assert_eq!(
        scanned,
        vec![
            ("run_node:a".to_owned(), "1".to_owned()),
            ("run_node:b".to_owned(), "2".to_owned()),
        ]
    );
    store.kv_delete("run_node:a").unwrap();
    store.kv_delete("run_node:a").unwrap();
    assert_eq!(store.kv_get("run_node:a").unwrap(), None);
    assert_eq!(store.kv_prefix("run_node:").unwrap().len(), 1);
    assert!(matches!(store.kv_prefix(" "), Err(ArchonError::Invalid(_))));
    assert!(matches!(store.kv_prefix("é"), Err(ArchonError::Invalid(_))));
}

#[test]
fn item_repos_names_each_repository_in_a_state_once() {
    let (mut store, alpha, items) = seeded(3);
    let beta = common::repo("beta");
    store.insert_repo(&beta).unwrap();
    store.insert_item(&item(None, "no repo"), None).unwrap();
    let busy = store.insert_item(&item(Some(&beta), "busy"), None).unwrap();
    start(&mut store, &busy.id);
    start(&mut store, &items[0].id);

    let mut queued = vec![None, Some(alpha.id.clone())];
    queued.sort();
    assert_eq!(store.item_repos("queued").unwrap(), queued);
    let mut running = vec![Some(alpha.id.clone()), Some(beta.id.clone())];
    running.sort();
    assert_eq!(store.item_repos("running").unwrap(), running);
    assert!(store.item_repos("blocked").unwrap().is_empty());
}
