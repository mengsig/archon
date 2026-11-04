//! Lessons 2.0 (V23): clusters, provenance refs and holdout-measured lift.
mod common;
use archon_core::config::LearningConfig;
use archon_core::id::QueueItemId;
use archon_core::lesson::{
    CONTRACT_LESSON_BYTES, CONTRACT_LESSON_NOTICE, CONTRACT_LESSONS, HOLDOUT_MIN_SAMPLES, Lesson,
    LessonKind, LessonRef, LessonRefKind, LessonScope, LessonSource, LessonState, held_out,
};
use archon_core::pull_request::PullRequestRef;
use archon_core::queue::{QueueItem, QueueState};
use archon_queue::Store;
use common::*;

const RATE: f64 = 0.2;

fn with_holdout(store: &Store, rate: f64) {
    let config = LearningConfig {
        holdout_rate: rate,
        ..LearningConfig::default()
    };
    store.configure_learning(&config).unwrap();
}

fn draft(scope: LessonScope, text: &str, source: LessonSource, state: LessonState) -> Lesson {
    Lesson::new(
        scope,
        LessonKind::Practice,
        text,
        source,
        state,
        archon_core::now(),
    )
    .unwrap()
}

fn active(store: &Store, text: &str) -> Lesson {
    let lesson = draft(
        LessonScope::default(),
        text,
        LessonSource::Orchestrator,
        LessonState::Active,
    );
    store.record_lesson(lesson).unwrap()
}

/// Runs `item` once, ending it as `won` says (`None`: cancelled), and returns
/// the ids of the lessons its contract carried.
fn run_once(store: &mut Store, item: &QueueItemId, won: Option<bool>) -> Vec<String> {
    let run = start(store, item);
    let given = store.lessons_for_run(&run.id, None, "direct").unwrap();
    let ending = match won {
        Some(true) => QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        Some(false) => QueueState::Failed {
            run: Some(run.id.clone()),
            error: "tests failed".into(),
        },
        None => QueueState::Cancelled { reason: None },
    };
    settle_run(store, &run, ending, t(30));
    store
        .capture_outcome(&store.run(&run.id).unwrap(), None, true)
        .unwrap();
    given.into_iter().map(|l| l.id.to_string()).collect()
}

/// New items until `want` of them hold `lesson` out and `want` do not.
fn items_in_both_arms(
    store: &mut Store,
    lesson: &Lesson,
    want: usize,
) -> (Vec<QueueItem>, Vec<QueueItem>) {
    let (mut held, mut given) = (Vec::new(), Vec::new());
    for n in 0..100 * want {
        if held.len() == want && given.len() == want {
            break;
        }
        let made = store
            .insert_item(&item(None, &format!("item {n}")), None)
            .unwrap();
        if held_out(&lesson.id, &made.id, RATE) {
            held.push(made);
        } else {
            given.push(made);
        }
    }
    held.truncate(want);
    given.truncate(want);
    assert_eq!(
        (held.len(), given.len()),
        (want, want),
        "a 20% rate fills both arms"
    );
    (held, given)
}

#[test]
fn a_held_out_lesson_is_never_given_to_its_item_and_a_rerun_agrees() {
    let mut store = store();
    with_holdout(&store, RATE);
    let lesson = active(&store, "Check migrations against old stores");
    let (held, given) = items_in_both_arms(&mut store, &lesson, 1);
    let (held, given) = (&held[0].id, &given[0].id);
    assert!(run_once(&mut store, held, Some(false)).is_empty());
    store
        .transition_item(held, QueueState::Queued, t(40))
        .unwrap();
    assert!(
        run_once(&mut store, held, Some(false)).is_empty(),
        "a rerun of the item holds the same lesson out"
    );
    assert_eq!(
        run_once(&mut store, given, Some(true)),
        vec![lesson.id.to_string()]
    );
    let scored = store.lesson(&lesson.id).unwrap();
    assert_eq!(
        (scored.uses, scored.wins, scored.state),
        (1, 1, LessonState::Active),
        "held-out runs never count for or against the lesson"
    );
}

#[test]
fn a_pinned_lesson_and_a_zero_rate_hold_nothing_out() {
    let mut store = store();
    let lesson = active(&store, "Check migrations against old stores");
    let (held, _) = items_in_both_arms(&mut store, &lesson, 1);
    assert_eq!(
        run_once(&mut store, &held[0].id, Some(true)),
        vec![lesson.id.to_string()],
        "the default rate is 0"
    );
    assert!(store.learning_stats(None).unwrap().holdout_lift.is_none());
    with_holdout(&store, RATE);
    let patch = archon_core::lesson::LessonPatch {
        pinned: Some(true),
        ..Default::default()
    };
    store.update_lesson(&lesson.id, patch).unwrap();
    let (held, _) = items_in_both_arms(&mut store, &lesson, 1);
    assert_eq!(
        run_once(&mut store, &held[0].id, Some(true)),
        vec![lesson.id.to_string()]
    );
}

#[test]
fn lift_is_reported_only_with_enough_scored_runs_in_both_arms() {
    let mut store = store();
    with_holdout(&store, RATE);
    let lesson = active(&store, "Check migrations against old stores");
    let min = usize::try_from(HOLDOUT_MIN_SAMPLES).unwrap();
    let (held, given) = items_in_both_arms(&mut store, &lesson, min + 1);
    for item in &given[..min] {
        assert_eq!(run_once(&mut store, &item.id, Some(true)).len(), 1);
    }
    // A cancelled run is in neither arm's count.
    assert!(run_once(&mut store, &held[min].id, None).is_empty());
    for (n, item) in held[..min].iter().enumerate() {
        assert!(store.learning_stats(None).unwrap().holdout_lift.is_none());
        assert!(run_once(&mut store, &item.id, Some(n % 2 == 0)).is_empty());
    }
    let lift = store.learning_stats(None).unwrap().holdout_lift.unwrap();
    let min = HOLDOUT_MIN_SAMPLES;
    assert_eq!((lift.uses, lift.wins), (min, min));
    assert_eq!((lift.holdout_uses, lift.holdout_wins), (min, min / 2));
    assert!((lift.lift() - 0.5).abs() < 1e-9);
    let scored = store.lesson(&lesson.id).unwrap();
    assert_eq!(
        (u64::from(scored.uses), u64::from(scored.wins)),
        (min, min),
        "the Wilson bound still reads only the runs the lesson was given"
    );
    let other = repo("elsewhere");
    store.insert_repo(&other).unwrap();
    assert!(
        store
            .learning_stats(Some(&other.id))
            .unwrap()
            .holdout_lift
            .is_none(),
        "another repository's runs measure nothing"
    );
}

/// Queued items for which `wanted` says whether each of `lessons` is held out.
fn items_where(
    store: &mut Store,
    lessons: &[&Lesson],
    wanted: &[bool],
    count: usize,
) -> Vec<QueueItemId> {
    assert_eq!(lessons.len(), wanted.len());
    let mut found = Vec::new();
    for n in 0..20_000 {
        if found.len() == count {
            break;
        }
        let made = store
            .insert_item(&item(None, &format!("pick {n}")), None)
            .unwrap();
        let held: Vec<bool> = lessons
            .iter()
            .map(|l| held_out(&l.id, &made.id, RATE))
            .collect();
        if held == wanted {
            found.push(made.id);
        }
    }
    assert_eq!(found.len(), count, "enough items match {wanted:?}");
    found
}

#[test]
fn lift_counts_scored_runs_not_lessons_and_never_pinned_ones() {
    let mut store = store();
    with_holdout(&store, RATE);
    let first = active(&store, "Check migrations against old stores");
    let second = active(&store, "Quote shell variables that hold file names");
    let half = usize::try_from(HOLDOUT_MIN_SAMPLES).unwrap() / 2;
    let both = [&first, &second];
    for item in items_where(&mut store, &both, &[true, true], half) {
        assert!(run_once(&mut store, &item, Some(false)).is_empty());
    }
    for item in items_where(&mut store, &both, &[false, false], half) {
        assert_eq!(run_once(&mut store, &item, Some(true)).len(), 2);
    }
    assert!(
        store.learning_stats(None).unwrap().holdout_lift.is_none(),
        "{half} runs an arm, however many lessons each, are too few"
    );

    let mut store = common::store();
    with_holdout(&store, RATE);
    let pinned = active(&store, "Check migrations against old stores");
    let patch = archon_core::lesson::LessonPatch {
        pinned: Some(true),
        ..Default::default()
    };
    let pinned = store.update_lesson(&pinned.id, patch).unwrap();
    let eligible = active(&store, "Quote shell variables that hold file names");
    let min = usize::try_from(HOLDOUT_MIN_SAMPLES).unwrap();
    for item in items_where(&mut store, &[&eligible], &[true], min) {
        assert_eq!(
            run_once(&mut store, &item, Some(true)),
            vec![pinned.id.to_string()]
        );
    }
    assert!(
        store.learning_stats(None).unwrap().holdout_lift.is_none(),
        "a pinned lesson given every run is no given arm"
    );
}

#[test]
fn the_injection_budget_is_unchanged() {
    assert_eq!((CONTRACT_LESSONS, CONTRACT_LESSON_BYTES), (6, 1200));
    let mut store = store();
    with_holdout(&store, RATE);
    let topics = [
        "Pin the toolchain before building releases",
        "Rebase stacked branches after their parent merges",
        "Keep socket paths short in temporary directories",
        "Quote shell variables that hold file names",
        "Prefer the cached index over a fresh clone",
        "Name every reviewer finding with its file and line",
        "Wait for the integration checks before merging",
        "Record benchmark numbers with the commit that made them",
        "Close the database before deleting its directory",
        "Retry a push once after an authentication failure",
    ];
    for topic in topics {
        active(&store, topic);
    }
    let all = store.lessons(&Default::default()).unwrap();
    assert!(all.iter().all(|l| l.cluster.is_none()), "distinct advice");
    for n in 0..20 {
        let made = store
            .insert_item(&item(None, &format!("item {n}")), None)
            .unwrap();
        let run = start(&mut store, &made.id);
        let given = store.lessons_for_run(&run.id, None, "direct").unwrap();
        let bytes: usize = given
            .iter()
            .map(|l| l.quoted_observation().unwrap().len())
            .sum();
        assert!(given.len() <= CONTRACT_LESSONS);
        assert!(CONTRACT_LESSON_NOTICE.len() + bytes <= CONTRACT_LESSON_BYTES);
    }
}

#[test]
fn near_duplicates_cluster_and_a_contract_carries_one_of_them() {
    let mut store = store();
    let first = active(
        &store,
        "Run the migration tests before shipping a schema change",
    );
    let second = active(
        &store,
        "Run the migration tests before you ship a schema change",
    );
    let other = active(&store, "Check the web bundle before a release");
    let key = first.id.clone().min(second.id.clone());
    for id in [&first.id, &second.id] {
        let stored = store.lesson(id).unwrap();
        assert_eq!(stored.cluster, Some(key.clone()));
    }
    assert_eq!(store.lesson(&second.id).unwrap().text, second.text);
    assert_eq!(store.lesson(&other.id).unwrap().cluster, None);
    let made = store.insert_item(&item(None, "one"), None).unwrap();
    let given = run_once(&mut store, &made.id, Some(true));
    assert_eq!(given.len(), 2, "one per cluster: {given:?}");
    assert!(given.contains(&other.id.to_string()));
    let orchestrator = store.orchestrator_lessons().unwrap();
    assert_eq!(orchestrator.len(), 2);
}

#[test]
fn clustering_never_raises_a_proposed_lesson() {
    let mut store = store();
    let alpha = repo("alpha");
    store.insert_repo(&alpha).unwrap();
    let scope = LessonScope {
        repo: Some(alpha.id.clone()),
        pipeline: None,
    };
    let accepted = draft(
        scope.clone(),
        "Run the migration tests before shipping a schema change",
        LessonSource::Operator,
        LessonState::Active,
    );
    let accepted = store.record_lesson(accepted).unwrap();
    let proposed = draft(
        scope,
        "Run the migration tests before you ship a schema change",
        LessonSource::Orchestrator,
        LessonState::Proposed,
    );
    let proposed = store.record_lesson(proposed).unwrap();
    assert!(proposed.cluster.is_some());
    assert_eq!(
        proposed.cluster,
        store.lesson(&accepted.id).unwrap().cluster
    );
    assert_eq!(
        (proposed.state, proposed.source),
        (LessonState::Proposed, LessonSource::Orchestrator)
    );
    store.refresh_lessons(archon_core::now()).unwrap();
    let after = store.lesson(&proposed.id).unwrap();
    assert_eq!(
        (after.state, after.confidence),
        (proposed.state, proposed.confidence)
    );
}

#[test]
fn a_lesson_keeps_its_item_run_and_pull_request_and_a_repeat_adds_support() {
    let mut store = store();
    let alpha = repo("alpha");
    store.insert_repo(&alpha).unwrap();
    let first = store
        .insert_item(&item(Some(&alpha), "first"), None)
        .unwrap();
    let run = start(&mut store, &first.id);
    let pr = PullRequestRef::from_url("https://github.com/acme/alpha/pull/7").unwrap();
    store.record_run_pull_request(&run.id, &pr, None).unwrap();
    let scope = LessonScope {
        repo: Some(alpha.id.clone()),
        pipeline: None,
    };
    let text = "Check migrations against old stores";
    let mut lesson = draft(
        scope.clone(),
        text,
        LessonSource::Orchestrator,
        LessonState::Proposed,
    );
    lesson.item = Some(first.id.clone());
    let recorded = store.record_lesson(lesson).unwrap();
    let kinds: Vec<_> = recorded.refs.iter().map(|r| r.kind).collect();
    assert_eq!(
        kinds,
        [LessonRefKind::Item, LessonRefKind::Run, LessonRefKind::Pr]
    );
    assert_eq!(recorded.refs[2].value, pr.url());
    assert_eq!(recorded.support, 1);

    let second = store
        .insert_item(&item(Some(&alpha), "second"), None)
        .unwrap();
    let mut repeat = draft(
        scope,
        text,
        LessonSource::Orchestrator,
        LessonState::Proposed,
    );
    repeat.item = Some(second.id.clone());
    let repeated = store.record_lesson(repeat).unwrap();
    assert_eq!(repeated.id, recorded.id);
    assert_eq!(repeated.support, 2);
    assert_eq!(
        (repeated.state, repeated.confidence),
        (recorded.state, recorded.confidence),
        "support is provenance, not standing"
    );
    assert_eq!(store.lesson(&recorded.id).unwrap(), repeated);
}

#[test]
fn a_distilled_lesson_names_its_distilling_run() {
    let store = store();
    let run = archon_core::id::PipelineRunId::new();
    let lesson = draft(
        LessonScope::default(),
        "Migration checks catch schema drift",
        LessonSource::Distilled,
        LessonState::Proposed,
    );
    store
        .apply_distillation(&run, vec![(lesson.clone(), None)], None)
        .unwrap();
    let stored = store.lesson(&lesson.id).unwrap();
    let expected = LessonRef {
        kind: LessonRefKind::Run,
        value: run.to_string(),
    };
    assert_eq!(stored.refs, vec![expected]);
    assert_eq!(stored.support, 0, "a run is not an independent item");
}

#[test]
fn noesis_is_never_involved() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let owned = [
        "archon-queue/src/store/lessons.rs",
        "archon-queue/src/store/lessons",
        "archon-core/src/lesson.rs",
        "archon-daemon/src/learned.rs",
        "archon-daemon/src/learned",
        "archon-queue/Cargo.toml",
    ];
    let mut read = 0;
    let mut pending: Vec<_> = owned.iter().map(|p| root.join(p)).collect();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap().to_lowercase();
        assert!(!text.contains("noesis"), "{} names Noesis", path.display());
        read += 1;
    }
    assert!(read >= 10, "the lesson sources were found: {read}");
}

const LESSON_ROWS: &str = "SELECT id, repo, pipeline, kind, state, pinned, uses, wins, confidence,
    source, superseded_by, created_at, updated_at, confirmed_at, data FROM lessons ORDER BY id";

fn rows(conn: &rusqlite::Connection, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let width = stmt.column_count();
    stmt.query_map([], |r| (0..width).map(|i| r.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Lessons as a V22 store holds them: no refs, support or cluster anywhere.
fn legacy_lessons() -> Vec<Lesson> {
    let mut used = draft(
        LessonScope::default(),
        "Check migrations against old stores",
        LessonSource::Orchestrator,
        LessonState::Active,
    );
    (used.uses, used.wins, used.confidence) = (7, 5, 0.42);
    used.item = Some(QueueItemId::new());
    used.last_used_at = Some(t(90));
    let mut pinned = draft(
        LessonScope {
            repo: None,
            pipeline: Some("direct".into()),
        },
        "Run the migration tests before shipping a schema change",
        LessonSource::Operator,
        LessonState::Active,
    );
    (pinned.pinned, pinned.confidence, pinned.confirmed_at) = (true, 1.0, Some(t(80)));
    let mut old = draft(
        LessonScope::default(),
        "Run the migration tests before you ship a schema change",
        LessonSource::Distilled,
        LessonState::Retracted,
    );
    old.superseded_by = Some(used.id.clone());
    vec![used, pinned, old]
}

fn insert_legacy(conn: &rusqlite::Connection, l: &Lesson) {
    let data = serde_json::to_string(l).unwrap();
    for key in ["refs", "support", "cluster"] {
        assert!(!data.contains(&format!("\"{key}\"")), "{data}");
    }
    let ts = |at: &archon_core::Timestamp| at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    conn.execute(
        "INSERT INTO lessons (id,repo,pipeline,kind,state,pinned,uses,wins,confidence,source,
         superseded_by,created_at,updated_at,confirmed_at,data)
         VALUES (?1,NULL,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        rusqlite::params![
            l.id.as_str(),
            l.scope.pipeline,
            l.kind.as_str(),
            l.state.as_str(),
            l.pinned,
            l.uses,
            l.wins,
            l.confidence,
            l.source.as_str(),
            l.superseded_by.as_ref().map(|s| s.as_str().to_owned()),
            ts(&l.created_at),
            ts(&l.updated_at),
            l.confirmed_at.as_ref().map(ts),
            data
        ],
    )
    .unwrap();
}

#[test]
fn existing_lessons_migrate_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    drop(Store::open(&path).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE lesson_trials; DROP INDEX lessons_by_cluster;
         ALTER TABLE lessons DROP COLUMN cluster;
         DELETE FROM schema_migrations WHERE version >= 23;",
    )
    .unwrap();
    let legacy = legacy_lessons();
    for lesson in &legacy {
        insert_legacy(&conn, lesson);
    }
    conn.execute(
        "INSERT INTO lesson_uses (lesson, run, used_at, won) VALUES (?1, 'prun_gone', 't', 1)",
        [legacy[0].id.as_str()],
    )
    .unwrap();
    let before = rows(&conn, LESSON_ROWS);
    let uses = rows(&conn, "SELECT * FROM lesson_uses ORDER BY lesson, run");
    assert_eq!(before.len(), legacy.len());
    drop(conn);

    let store = Store::open(&path).unwrap();
    for lesson in &legacy {
        assert_eq!(&store.lesson(&lesson.id).unwrap(), lesson);
    }
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        rows(&conn, LESSON_ROWS),
        before,
        "every lesson row is byte-for-byte the same"
    );
    assert_eq!(
        rows(&conn, "SELECT * FROM lesson_uses ORDER BY lesson, run"),
        uses
    );
    let clusters = rows(
        &conn,
        "SELECT COUNT(*) FROM lessons WHERE cluster IS NOT NULL",
    );
    assert_eq!(clusters, vec![vec![rusqlite::types::Value::Integer(0)]]);
    let latest = rows(&conn, "SELECT MAX(version) FROM schema_migrations");
    assert_eq!(latest, vec![vec![rusqlite::types::Value::Integer(23)]]);
}
