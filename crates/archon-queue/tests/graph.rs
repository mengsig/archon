//! The `task_graph` projection (CONTRACTS.md §1.3): typed edges, filters,
//! byte-identical output for one store, and an `as_of` read in the same
//! transaction as the rows, under concurrent writes.

mod common;

use std::collections::BTreeMap;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::initiative::{EnqueueInitiative, Initiative, InitiativeMember};
use archon_core::protocol::github_links::LinkSource;
use archon_core::protocol::graph::TaskEdgeKind;
use archon_core::protocol::{
    ChangeSubject, GithubKind, GithubLink, GithubRef, LinkRelation, QueueStateKind, RepoRef,
    TaskFilter, TaskGraph, WaitingReasonCode,
};
use archon_core::queue::{Priority, QueueItem, QueueState};
use archon_queue::Store;
use archon_queue::store::SpendRow;
use archon_queue::store::stacks::StackParent;
use common::*;

fn graph(store: &Store, filter: &TaskFilter) -> TaskGraph {
    store.task_graph(filter, Some(2000), true).unwrap()
}

fn ids(graph: &TaskGraph) -> Vec<&QueueItemId> {
    graph.nodes.iter().map(|n| &n.id).collect()
}

fn edges(graph: &TaskGraph) -> Vec<(&QueueItemId, &QueueItemId, TaskEdgeKind, bool)> {
    graph
        .edges
        .iter()
        .map(|e| (&e.from, &e.to, e.kind, e.satisfied))
        .collect()
}

/// `title` created at `t(at)`, depending on `deps`.
fn insert_at(
    store: &mut Store,
    repo: &archon_core::repo::Repo,
    title: &str,
    at: i64,
    deps: &[&QueueItem],
) -> QueueItem {
    let mut new = item(Some(repo), title);
    new.created_at = t(at);
    new.updated_at = t(at);
    new.dependencies = deps.iter().map(|d| d.id.clone()).collect();
    store.insert_item(&new, None).unwrap()
}

fn member(after: &[usize]) -> InitiativeMember {
    InitiativeMember {
        repo: RepoRef("alpha".into()),
        brief: "a member".into(),
        title: None,
        pipeline: None,
        model_plan: None,
        autonomy: Default::default(),
        after: after.to_vec(),
        onto: None,
    }
}

/// An initiative of two members, the second after the first.
fn initiative(store: &mut Store, repo: &archon_core::repo::Repo) -> (Initiative, [QueueItem; 2]) {
    let made = Initiative::new("ship both", "both or neither", true, t(30)).unwrap();
    let mut first = item(Some(repo), "member one");
    let mut second = item(Some(repo), "member two");
    for (n, member) in [&mut first, &mut second].into_iter().enumerate() {
        member.initiative = Some(made.id.clone());
        member.created_at = t(31 + n as i64);
        member.updated_at = member.created_at;
    }
    second.dependencies = vec![first.id.clone()];
    let request = EnqueueInitiative {
        title: made.title.clone(),
        contract: made.contract.clone(),
        atomic: true,
        members: vec![member(&[]), member(&[0])],
        idempotency_key: None,
    };
    store
        .insert_initiative(&made, &[first.clone(), second.clone()], &request)
        .unwrap();
    (made, [first, second])
}

fn stack(child: &QueueItem, parent: &QueueItem, merged: bool) -> StackParent {
    StackParent {
        item: child.id.clone(),
        parent_item: parent.id.clone(),
        parent_branch: "archon/parent".into(),
        parent_sha: "a".repeat(40),
        position: 0,
        repository_base: None,
        merged,
        closed: false,
        conflict: None,
        needs_attention: None,
        landed: None,
    }
}

fn charge(store: &mut Store, n: u32, item: &QueueItem, usd: (f64, f64)) {
    let row = SpendRow {
        agent: agent(n),
        item: Some(item.id.clone()),
        cost_usd: usd.0,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: usd.1,
        input_tokens: 1,
        output_tokens: 1,
        at: t(40),
    };
    store.record_spend(&row).unwrap();
}

#[test]
fn edges_are_typed_and_ordered_and_only_join_the_nodes() {
    let (mut store, repo, _) = seeded(0);
    let base = insert_at(&mut store, &repo, "base", 10, &[]);
    let child = insert_at(&mut store, &repo, "child", 11, &[&base]);
    store
        .save_stack_parent(&stack(&child, &base, false))
        .unwrap();
    let (made, [first, second]) = initiative(&mut store, &repo);

    let all = graph(&store, &TaskFilter::default());
    assert_eq!(ids(&all), [&base.id, &child.id, &first.id, &second.id]);
    let mut expected = vec![
        (&base.id, &child.id, TaskEdgeKind::DependsOn, false),
        (&base.id, &child.id, TaskEdgeKind::StackedOn, false),
        (&first.id, &second.id, TaskEdgeKind::InitiativeAfter, false),
    ];
    expected.sort();
    assert_eq!(edges(&all), expected);
    assert_eq!(all.initiatives.len(), 1);
    assert_eq!(
        (&all.initiatives[0].id, all.initiatives[0].atomic),
        (&made.id, true)
    );
    assert_eq!(all.as_of, store.last_change_seq().unwrap());
    assert!(!all.truncated);

    // The prerequisite succeeds: it leaves the graph, and so does its edge.
    let run = start(&mut store, &base.id);
    let done = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "done".into(),
    };
    settle_run(&mut store, &run, done, t(50));
    store
        .save_stack_parent(&stack(&child, &base, true))
        .unwrap();
    let active = graph(&store, &TaskFilter::default());
    assert_eq!(ids(&active), [&child.id, &first.id, &second.id]);
    assert!(active.edges.iter().all(|e| e.from != base.id));
    let since = TaskFilter {
        include_terminal_since: Some(t(50)),
        ..TaskFilter::default()
    };
    let with_done = graph(&store, &since);
    assert_eq!(ids(&with_done)[0], &base.id, "it changed at t(50)");
    let satisfied: Vec<_> = edges(&with_done)
        .into_iter()
        .filter(|e| e.0 == &base.id)
        .map(|e| (e.2, e.3))
        .collect();
    assert_eq!(
        satisfied,
        [
            (TaskEdgeKind::DependsOn, true),
            (TaskEdgeKind::StackedOn, true)
        ]
    );
    let later = TaskFilter {
        include_terminal_since: Some(t(51)),
        ..TaskFilter::default()
    };
    assert!(!ids(&graph(&store, &later)).contains(&&base.id));
}

#[test]
fn nodes_carry_their_stage_agents_spend_links_and_blocked_reason() {
    let (mut store, repo, _) = seeded(0);
    let failed = insert_at(&mut store, &repo, "cancelled dep", 10, &[]);
    let blocked = insert_at(&mut store, &repo, "blocked", 11, &[&failed]);
    let running = insert_at(&mut store, &repo, "running", 12, &[]);
    store
        .transition_item(
            &failed.id,
            QueueState::Cancelled {
                reason: Some("dropped".into()),
            },
            t(20),
        )
        .unwrap();
    store
        .transition_item(
            &blocked.id,
            QueueState::Blocked {
                reason: "its dependency failed".into(),
            },
            t(21),
        )
        .unwrap();
    let run = start(&mut store, &running.id);
    store.upsert_stage_run(&running_stage(&run, 0, 1)).unwrap();
    store.upsert_stage_run(&running_stage(&run, 1, 2)).unwrap();
    charge(&mut store, 1, &running, (1.25, 0.5));
    charge(&mut store, 2, &running, (0.25, 0.0));
    let issue = GithubRef {
        host: "github.com".into(),
        repo: "o/r".into(),
        number: 7,
        kind: GithubKind::Issue,
    };
    let closes = GithubLink {
        item: running.id.clone(),
        reference: issue.clone(),
        relation: LinkRelation::Closes,
        source: LinkSource::Operator,
        created_at: t(22),
    };
    store.link_github(&closes).unwrap();

    let all = graph(&store, &TaskFilter::default());
    let by_id: BTreeMap<_, _> = all.nodes.iter().map(|n| (&n.id, n)).collect();
    assert!(!by_id.contains_key(&failed.id), "terminal by default");
    let stuck = by_id[&blocked.id];
    assert_eq!(stuck.state, QueueStateKind::Blocked);
    assert_eq!(stuck.state_detail.as_deref(), Some("its dependency failed"));
    assert_eq!(
        stuck.waiting_reason,
        Some(WaitingReasonCode::DependencyFailed)
    );
    let busy = by_id[&running.id];
    assert_eq!(busy.stage.as_deref(), Some("implement"));
    assert_eq!(busy.agents_running, 2);
    assert_eq!(busy.spent_usd, 2.0);
    assert_eq!(busy.issue.as_ref(), Some(&issue));
    assert_eq!(busy.waiting_reason, None, "the daemon adds a gate's reason");
    assert_eq!(
        store.spend_for_item(&running.id, true).unwrap(),
        busy.spent_usd
    );
    let actual = store
        .task_graph(&TaskFilter::default(), None, false)
        .unwrap();
    let busy = actual.nodes.iter().find(|n| n.id == running.id).unwrap();
    assert_eq!(busy.spent_usd, 1.5, "estimates only when counted");
}

fn pick(store: &Store, filter: TaskFilter) -> Vec<QueueItemId> {
    ids(&graph(store, &filter)).into_iter().cloned().collect()
}

#[test]
fn every_filter_narrows_the_nodes() {
    let (mut store, repo, _) = seeded(0);
    let other = common::repo("beta");
    store.insert_repo(&other).unwrap();
    let a = insert_at(&mut store, &repo, "a", 10, &[]);
    let b = insert_at(&mut store, &repo, "b", 11, &[&a]);
    let mut urgent = item(Some(&other), "urgent");
    urgent.priority = Priority::High;
    urgent.pipeline = "review".into();
    urgent.created_at = t(12);
    let urgent = store.insert_item(&urgent, None).unwrap();
    let (made, [first, second]) = initiative(&mut store, &repo);
    let lone = insert_at(&mut store, &repo, "lone", 40, &[]);

    let repos = TaskFilter {
        repos: Some(vec![other.id.clone()]),
        ..TaskFilter::default()
    };
    assert_eq!(pick(&store, repos), [urgent.id.clone()]);
    let pipelines = TaskFilter {
        pipelines: Some(vec!["review".into()]),
        ..TaskFilter::default()
    };
    assert_eq!(pick(&store, pipelines), [urgent.id.clone()]);
    let priority = TaskFilter {
        min_priority: Some(Priority::High),
        ..TaskFilter::default()
    };
    assert_eq!(pick(&store, priority), [urgent.id.clone()]);
    let members = TaskFilter {
        initiative: Some(made.id.clone()),
        ..TaskFilter::default()
    };
    assert_eq!(pick(&store, members), [first.id.clone(), second.id.clone()]);
    let component = TaskFilter {
        component_of: Some(b.id.clone()),
        ..TaskFilter::default()
    };
    assert_eq!(pick(&store, component), [a.id.clone(), b.id.clone()]);
    let alone = TaskFilter {
        component_of: Some(lone.id.clone()),
        ..TaskFilter::default()
    };
    assert_eq!(pick(&store, alone), [lone.id.clone()]);
    store
        .transition_item(&a.id, QueueState::Dispatching, t(41))
        .unwrap();
    let states = TaskFilter {
        states: Some(vec![QueueStateKind::Dispatching]),
        ..TaskFilter::default()
    };
    assert_eq!(pick(&store, states), [a.id.clone()]);
}

#[test]
fn a_bad_filter_or_limit_is_refused_and_a_limit_truncates() {
    let (store, _, items) = seeded(3);
    let empty = TaskFilter {
        states: Some(Vec::new()),
        ..TaskFilter::default()
    };
    let refused = store.task_graph(&empty, None, true).unwrap_err();
    assert!(matches!(&refused, ArchonError::Invalid(m) if m.contains("states")));
    let unknown = TaskFilter {
        component_of: Some(QueueItemId::new()),
        ..TaskFilter::default()
    };
    assert!(matches!(
        store.task_graph(&unknown, None, true),
        Err(ArchonError::NotFound(_))
    ));
    let nameless = TaskFilter {
        initiative: Some(archon_core::id::InitiativeId::new()),
        ..TaskFilter::default()
    };
    assert!(matches!(
        store.task_graph(&nameless, None, true),
        Err(ArchonError::NotFound(_))
    ));
    for bad in [0, 2001] {
        let refused = store.task_graph(&TaskFilter::default(), Some(bad), true);
        assert!(matches!(refused, Err(ArchonError::Invalid(_))), "{bad}");
    }
    let two = store
        .task_graph(&TaskFilter::default(), Some(2), true)
        .unwrap();
    assert!(two.truncated);
    assert_eq!(two.nodes.len(), 2);
    // Same creation time: the id breaks the tie.
    let mut sorted: Vec<_> = items.iter().map(|i| &i.id).collect();
    sorted.sort();
    assert_eq!(ids(&two), sorted[..2]);
    let all = store
        .task_graph(&TaskFilter::default(), Some(3), true)
        .unwrap();
    assert!(!all.truncated);
}

#[test]
fn the_same_store_gives_the_same_bytes_on_every_connection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut writer = Store::open(&path).unwrap();
    let repo = repo("alpha");
    writer.insert_repo(&repo).unwrap();
    let base = insert_at(&mut writer, &repo, "base", 10, &[]);
    let child = insert_at(&mut writer, &repo, "child", 11, &[&base]);
    writer
        .save_stack_parent(&stack(&child, &base, false))
        .unwrap();
    initiative(&mut writer, &repo);
    let run = start(&mut writer, &child.id);
    writer.upsert_stage_run(&running_stage(&run, 0, 1)).unwrap();
    charge(&mut writer, 1, &child, (0.1, 0.2));

    let bytes = |store: &Store| serde_json::to_vec(&graph(store, &TaskFilter::default())).unwrap();
    let first = bytes(&writer);
    assert_eq!(bytes(&writer), first, "a second read");
    let reader = writer.open_reader(&path).unwrap();
    assert_eq!(bytes(&reader), first, "another connection");
    drop((reader, writer));
    let reopened = Store::open(&path).unwrap();
    assert_eq!(bytes(&reopened), first, "after a reopen");
}

/// What the changes up to `as_of` say each item's state is.
fn replayed_states(store: &Store, as_of: u64) -> BTreeMap<QueueItemId, QueueStateKind> {
    let mut states = BTreeMap::new();
    let mut after = 0;
    while after < as_of {
        let page = store.changes_after(after, 1000).unwrap();
        assert!(!page.is_empty(), "changes up to {as_of} are kept");
        for change in page.iter().take_while(|c| c.seq <= as_of) {
            if let ChangeSubject::Item { item, state, .. } = &change.subject {
                states.insert(item.clone(), *state);
            }
            after = change.seq;
        }
        if page.last().is_some_and(|c| c.seq > as_of) {
            break;
        }
    }
    states
}

/// A writer enqueues and dispatches items while a reader takes graphs. Each
/// graph holds exactly the items whose changes it is `as_of`, in the states
/// those changes give, so a client applying `seq > as_of` loses nothing.
#[test]
fn as_of_is_the_change_seq_the_graph_was_read_at_under_concurrent_writes() {
    const WRITES: usize = 120;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut writer = Store::open(&path).unwrap();
    let repo = repo("alpha");
    writer.insert_repo(&repo).unwrap();
    let reader = writer.open_reader(&path).unwrap();
    let writing = std::thread::spawn(move || {
        for n in 0..WRITES {
            let new = writer
                .insert_item(&item(Some(&repo), &format!("item {n}")), None)
                .unwrap();
            if n % 3 == 0 {
                writer
                    .transition_item(&new.id, QueueState::Dispatching, t(5))
                    .unwrap();
            }
        }
        writer
    });
    let mut seen = Vec::new();
    while !writing.is_finished() || seen.len() < 2 {
        let read = graph(&reader, &TaskFilter::default());
        let nodes: BTreeMap<_, _> = read.nodes.iter().map(|n| (n.id.clone(), n.state)).collect();
        assert_eq!(
            nodes,
            replayed_states(&reader, read.as_of),
            "as of {}",
            read.as_of
        );
        seen.push(read.as_of);
    }
    let writer = writing.join().unwrap();
    assert!(
        seen.windows(2).all(|w| w[0] <= w[1]),
        "as_of never goes back"
    );
    let last = graph(&writer, &TaskFilter::default());
    assert_eq!(last.nodes.len(), WRITES);
    assert_eq!(last.as_of, writer.last_change_seq().unwrap());
}
