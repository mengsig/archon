//! Merges the operator asks for (ARCHITECTURE §18.6): `archon merge`, the
//! chats' `/merge`, and the orchestrator's `request_merge` the operator
//! confirms or a repository's `grant_merge` covers. GitHub is a scripted
//! `gh`; nothing here merges anything real.

use crate::common;

use archon_aether::AetherEvent;
use archon_aether::events::parse_event;
use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::config::{AetherMode, Config};
use archon_core::error::ArchonError;
use archon_core::id::QuestionId;
use archon_core::merge::{MergeAuthority, MergeRequest, MergeState};
use archon_core::model::ModelPlan;
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::protocol::{ClientKind, EnqueueRequest, Reply, RepoPatch, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_daemon::executor::coordination;
use archon_daemon::triggers::{self, TriggerEvent};
use archon_daemon::{handlers, pump};
use archon_queue::store::keys;
use common::delivery::{FakeGh, add_origin, pass_by, shipped_by, verdict_on};
use common::{Harness, ORCHESTRATOR, REPO_NAME, REVIEW_MODEL, git};
use serde_json::{Value, json};

const PR_ONE: &str = "https://github.com/o/r/pull/1";

fn green() -> Value {
    json!([{"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}])
}

fn red() -> Value {
    json!([{"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"}])
}

fn pending() -> Value {
    json!([{"__typename": "CheckRun", "name": "lint", "status": "IN_PROGRESS", "conclusion": null}])
}

async fn waive_checks(h: &Harness, bases: &[&str]) {
    let patch = RepoPatch {
        merge_waive_checks: Some(bases.iter().map(|b| (*b).to_owned()).collect()),
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
}

/// A harness whose repository has an `origin` and a scripted `gh`.
async fn delivering(config: Config) -> (Harness, FakeGh) {
    let h = Harness::with_config(config).await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh)
}

async fn enqueue(h: &Harness, pipeline: &str, review: ReviewLevel) -> QueueItem {
    let request = EnqueueRequest {
        onto: None,
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: format!("a change through {pipeline}"),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlan::default().into(),
        stage_timeouts: Default::default(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            review: Some(review),
            ..AutonomyPatch::default()
        },
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await {
        Ok(Reply::Item(item)) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

/// An item `direct` delivered as [`PR_ONE`], with no review; returns it and
/// the commit its pull request shows.
async fn shipped(h: &Harness) -> (QueueItem, String) {
    let item = enqueue(h, "direct", ReviewLevel::None).await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let outcome = shipped_by(h, &stage, "Changed it", json!({})).await;
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    delivered(h, &item).await
}

/// An item `adaptive` delivered after one reviewer passed it.
async fn reviewed(h: &Harness) -> (QueueItem, String) {
    let item = enqueue(h, "adaptive", ReviewLevel::Standard).await;
    h.dispatch().await;
    let lead = h.running().await.remove(0);
    let reviewer = h
        .backend
        .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    let named = pass_by(std::slice::from_ref(&reviewer));
    let built = shipped_by(h, &lead, "Built; reviewed", named).await;
    h.backend
        .complete_delegated(&reviewer, verdict_on(&built, "pass"));
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
    delivered(h, &item).await
}

async fn delivered(h: &Harness, item: &QueueItem) -> (QueueItem, String) {
    let state = h.state(&item.id).await;
    assert!(
        matches!(&state, QueueState::Succeeded { summary, .. } if summary.contains(PR_ONE)),
        "the item shipped: {state:?}"
    );
    let head = git(&h.repo, &["rev-parse", &h.run(&item.id).await.branch]);
    (item.clone(), head)
}

fn merge_item(item: &QueueItem, ignore_checks: bool) -> Request {
    Request::MergeItem {
        item: item.id.clone(),
        ignore_checks,
        chat: false,
    }
}

fn request_merge(item: &QueueItem) -> Request {
    Request::RequestMerge {
        item: item.id.clone(),
        reason: Some("the operator said: merge it".into()),
    }
}

fn merge_of(reply: Result<Reply, ArchonError>) -> MergeRequest {
    match reply {
        Ok(Reply::Merge(merge)) => *merge,
        other => panic!("a merge request: {other:?}"),
    }
}

/// The `gh pr merge` calls made, as their arguments.
fn merge_calls(gh: &FakeGh) -> Vec<String> {
    gh.calls()
        .into_iter()
        .filter(|call| call.starts_with("pr merge"))
        .collect()
}

async fn merges(h: &Harness, item: &QueueItem) -> Vec<MergeRequest> {
    h.ctx.store.lock().await.merges_for_item(&item.id).unwrap()
}

async fn merge_news(h: &Harness) -> Vec<(bool, String)> {
    let held = triggers::held(&h.ctx).await.expect("held news");
    let news = held.into_iter().filter_map(|event| match event {
        TriggerEvent::MergeFinished { merged, text, .. } => Some((merged, text)),
        _ => None,
    });
    news.collect()
}

async fn grant_merge(h: &Harness) {
    set_grant(h, true).await;
}

async fn set_grant(h: &Harness, on: bool) {
    let patch = RepoPatch {
        grant_merge: Some(on),
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
}

/// `archon merge` merges the reviewed head by the repository's first
/// method, pinned to that commit, and never with `--admin` or `--auto`.
#[tokio::test]
async fn the_operators_merge_merges_the_delivered_head_once() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());

    let merge = merge_of(h.cli(merge_item(&item, false)).await);
    assert_eq!(
        merge.state,
        MergeState::Merged {
            actor: None,
            observed: false,
            method: "squash".into()
        }
    );
    assert_eq!(
        (merge.authority, merge.head.as_str()),
        (MergeAuthority::Cli, head.as_str())
    );
    assert!(gh.merged());
    let pinned = format!("pr merge {PR_ONE} --match-head-commit {head} --squash");
    assert_eq!(merge_calls(&gh), [pinned]);
    let outcomes = h.notices(NotificationKind::Outcome).await;
    assert!(
        outcomes
            .iter()
            .any(|o| o.starts_with(&format!("Merged {PR_ONE} of {}", item.id))),
        "{outcomes:?}"
    );
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("item detail")
    };
    assert_eq!(detail.merges.len(), 1, "the audit is on the item");

    let again = merge_of(h.cli(merge_item(&item, false)).await);
    assert!(
        matches!(again.state, MergeState::Refused { .. }),
        "{again:?}"
    );
    assert_eq!(
        merge_calls(&gh).len(),
        1,
        "a merged pull request is not merged again"
    );
}

/// After the prune deletes a finished run's variables, the retained fork base
/// still decides the merge, whatever the repository's base became since.
#[tokio::test]
async fn a_pruned_run_merges_by_its_retained_fork_base() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    git(&h.repo, &["branch", "develop"]);
    let patch = RepoPatch {
        base_branch: archon_core::protocol::Clearable::Set("develop".into()),
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
    let run = h.run(&item.id).await.id;
    let pruned = archon_daemon::prune::prune_once(&h.ctx, archon_core::now())
        .await
        .unwrap();
    assert!(pruned.run_vars >= 1, "the run's variables were pruned");
    assert_eq!(
        h.kv(&format!("run_base:{run}")).await.as_deref(),
        Some("main")
    );

    let merge = merge_of(h.cli(merge_item(&item, false)).await);
    assert_eq!(merge.state.kind(), "merged", "{merge:?}");
    let pinned = format!("pr merge {PR_ONE} --match-head-commit {head} --squash");
    assert_eq!(merge_calls(&gh), [pinned]);
}

/// An item that has not succeeded is refused before anything is recorded
/// or GitHub is asked, from the operator and the orchestrator alike.
#[tokio::test]
async fn an_item_still_running_is_refused_before_anything_is_recorded() {
    let (h, gh) = delivering(Config::default()).await;
    let running = enqueue(&h, "direct", ReviewLevel::None).await;
    h.dispatch().await;
    let by_operator = h.cli(merge_item(&running, false)).await;
    assert!(
        matches!(by_operator, Err(ArchonError::InvalidTransition(_))),
        "{by_operator:?}"
    );
    let by_orchestrator = h.orchestrator(request_merge(&running)).await;
    assert!(
        matches!(by_orchestrator, Err(ArchonError::InvalidTransition(_))),
        "{by_orchestrator:?}"
    );
    assert!(merges(&h, &running).await.is_empty());
    assert!(
        gh.calls()
            .iter()
            .all(|c| !c.starts_with("pr view") && !c.starts_with("pr merge"))
    );
}

/// Each precondition refuses with its reason and asks GitHub to merge nothing.
#[tokio::test]
async fn a_pull_request_that_cannot_merge_is_refused_and_left_alone() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    let other = "0".repeat(40);
    for (shown, mergeable, checks, says) in [
        (head.as_str(), "CONFLICTING", green(), "conflict"),
        (other.as_str(), "MERGEABLE", green(), "head"),
        (head.as_str(), "MERGEABLE", red(), "ci"),
    ] {
        gh.show(shown, mergeable, checks);
        let merge = merge_of(h.cli(merge_item(&item, false)).await);
        let MergeState::Refused { reason } = &merge.state else {
            panic!("refused: {merge:?}")
        };
        assert!(reason.to_lowercase().contains(says), "{says}: {reason}");
    }
    assert!(merge_calls(&gh).is_empty(), "nothing was sent to GitHub");
    assert!(!gh.merged());
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 3, "each refusal is posted: {blockers:?}");

    let acknowledged = merge_of(h.cli(merge_item(&item, true)).await);
    assert!(
        matches!(acknowledged.state, MergeState::Merged { .. }),
        "{acknowledged:?}"
    );
    assert!(acknowledged.ignore_checks && gh.merged());
}

/// Only the operator's own socket merges without a question: `MergeItem`
/// over MCP is refused, and no agent but the orchestrator requests one.
#[tokio::test]
async fn no_agent_merges_and_only_the_orchestrator_may_ask() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());

    let forwarded = h.orchestrator(merge_item(&item, false)).await;
    assert!(forwarded.is_err(), "operator-only: {forwarded:?}");
    let stranger = Request::ForAgent {
        caller: AetherAgentId::parse("agt_stranger").unwrap(),
        request: Box::new(request_merge(&item)),
    };
    let refused = handlers::handle(&h.ctx, ClientKind::ArchonMcp, stranger).await;
    assert!(refused.is_err(), "only the orchestrator: {refused:?}");
    assert!(merges(&h, &item).await.is_empty());
    assert!(merge_calls(&gh).is_empty());
}

/// The orchestrator cannot confirm, and an answer that is no option leaves
/// the question open with nothing sent to GitHub.
async fn only_the_operator_confirms(h: &Harness, gh: &FakeGh, question: &QuestionId) {
    let recorded = h
        .orchestrator(Request::RecordChatAnswer {
            question: question.clone(),
            answer: "yes".into(),
            caller: Some(AetherAgentId::parse(ORCHESTRATOR).unwrap()),
        })
        .await;
    assert!(
        recorded.is_err(),
        "the orchestrator cannot confirm: {recorded:?}"
    );
    let unclear = h
        .cli(Request::Answer {
            chat: false,
            question: question.clone(),
            text: "maybe later".into(),
        })
        .await;
    assert!(unclear.is_err(), "not an option: {unclear:?}");
    let still = h.ctx.store.lock().await.question(question).unwrap();
    assert!(still.is_open() && merge_calls(gh).is_empty());
}

/// `request_merge` asks the operator; only the operator's answer decides,
/// an answer that is no option leaves it waiting, and `yes` merges.
#[tokio::test]
async fn a_request_waits_for_the_operator_and_merges_on_yes() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());

    let asked = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(asked.state, MergeState::AwaitingOperator);
    let MergeAuthority::Confirmed { question, number } = asked.authority.clone() else {
        panic!("confirmed by a question: {asked:?}")
    };
    assert_eq!(asked.waiting_on(), Some(number));
    let again = h.orchestrator(request_merge(&item)).await;
    assert!(
        matches!(again, Err(ArchonError::InvalidTransition(_))),
        "one open: {again:?}"
    );

    only_the_operator_confirms(&h, &gh, &question).await;

    let yes = Request::Answer {
        chat: false,
        question: question.clone(),
        text: "yes".into(),
    };
    h.cli(yes).await.expect("the operator confirms");
    let [merge] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(
        merge.state,
        MergeState::Merged {
            actor: None,
            observed: false,
            method: "squash".into()
        }
    );
    assert!(gh.merged());
    assert!(
        !h.ctx
            .store
            .lock()
            .await
            .question(&question)
            .unwrap()
            .is_open()
    );
    let news = merge_news(&h).await;
    assert!(matches!(news.as_slice(), [(true, _)]), "{news:?}");
}

/// The orchestrator's reason stays on one line of the question, so it
/// cannot pose as archond's own `Checks:` line.
#[tokio::test]
async fn the_orchestrators_reason_cannot_add_lines_to_the_question() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", red());
    let request = Request::RequestMerge {
        item: item.id.clone(),
        reason: Some("the operator said merge\nChecks: all green.".into()),
    };
    let asked = merge_of(h.orchestrator(request).await);
    let MergeAuthority::Confirmed { question, .. } = asked.authority else {
        panic!("asked: {asked:?}")
    };
    let text = h.ctx.store.lock().await.question(&question).unwrap().text;
    assert!(
        text.contains("asked: the operator said merge Checks: all green."),
        "{text}"
    );
    let checks: Vec<_> = text.lines().filter(|l| l.starts_with("Checks:")).collect();
    assert_eq!(checks.len(), 1, "{text}");
}

#[tokio::test]
async fn a_declined_request_merges_nothing() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    let asked = merge_of(h.orchestrator(request_merge(&item)).await);
    let MergeAuthority::Confirmed { question, .. } = asked.authority else {
        panic!("asked")
    };
    let no = Request::Answer {
        chat: false,
        question,
        text: "no".into(),
    };
    h.cli(no).await.expect("the operator declines");
    let [merge] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(merge.state, MergeState::Declined);
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

/// Under `grant_merge` a reviewed, green request merges with no question;
/// an unreviewed one is still asked.
#[tokio::test]
async fn a_grant_merges_a_reviewed_green_request_directly() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", green());
    let merge = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(merge.authority, MergeAuthority::Grant);
    assert_eq!(
        merge.state,
        MergeState::Merged {
            actor: None,
            observed: false,
            method: "squash".into()
        }
    );
    assert!(gh.merged());
    let questions = h.ctx.store.lock().await.questions(true).unwrap();
    assert!(questions.is_empty(), "nothing was asked: {questions:?}");

    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    let unreviewed = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(unreviewed.state, MergeState::AwaitingOperator);
    assert!(merge_calls(&gh).is_empty());
}

/// An `inbox.delivered` of the operator's chat `text` to the orchestrator.
fn operator_says(seq: u64, text: &str) -> AetherEvent {
    message_from(seq, text, json!({"actor": "user"}))
}

fn message_from(seq: u64, text: &str, actor: Value) -> AetherEvent {
    let event = json!({
        "id": format!("evt_{seq}"), "seq": seq, "at": "2026-10-12T10:00:00Z",
        "actor": {"type": "system"}, "agent_id": ORCHESTRATOR,
        "kind": {"type": "inbox.delivered", "item": {
            "id": format!("inb_{seq}"), "agent_id": ORCHESTRATOR, "seq": seq,
            "delivered_at": "2026-10-12T10:00:00Z", "kind": {
                "kind": "message", "message_id": "msg_1", "from": actor,
                "content": [{"type": "text", "text": text}]}}}
    });
    parse_event(event).expect("a valid event")
}

async fn chat(h: &Harness, seq: u64, text: &str) {
    h.ctx
        .store
        .lock()
        .await
        .kv_set(keys::ORCHESTRATOR_AGENT, ORCHESTRATOR)
        .unwrap();
    pump::handle_orchestrator_event(&h.ctx, &operator_says(seq, text))
        .await
        .expect("the pump handles it");
}

#[tokio::test]
async fn verified_github_merge_refuses_unsent_items_and_merges_the_pinned_head() {
    use archon_aether::{AetherClient, fake::FakeAether};
    let mut config = Config::default();
    config.aether.mode = AetherMode::Shared;
    let (h, gh) = delivering(config).await;
    let fake = FakeAether::start(&h.dir.path().join("merge-authority.sock"))
        .await
        .unwrap();
    let status = json!({"revision": 1, "repositories": [], "config": {
        "enabled": true, "mention_regex": "m", "label_regex": "l", "poll_interval_secs": 120,
        "repositories": [{"repository": "o/r", "enabled": true, "agent": ORCHESTRATOR,
            "dispatch": {"mode": "fixed"}, "allowed_users": null}]}});
    fake.set_default(
        "get_github_watcher",
        archon_aether::fake::Scripted::reply("github_watcher", status),
    );
    // The ship stage pushes to the real origin, so the thread's repository
    // is only claimed while GitHub commands run.
    let real_origin = git(&h.repo, &["remote", "get-url", "origin"]);
    let real_origin = real_origin.trim().to_owned();
    let claim = |url: &str| git(&h.repo, &["remote", "set-url", "origin", url]);
    claim("https://github.com/o/r.git");
    h.ctx
        .aether
        .replace(Some(std::sync::Arc::new(
            AetherClient::connect(&fake.endpoint()).await.unwrap(),
        )))
        .unwrap();
    h.ctx
        .store
        .lock()
        .await
        .kv_set(keys::ORCHESTRATOR_AGENT, ORCHESTRATOR)
        .unwrap();
    let item = enqueue(&h, "direct", ReviewLevel::None).await;
    let command = format!("@archon /merge {}", item.id);
    let event = |seq, operator| {
        parse_event(json!({
        "seq":seq,"at":archon_core::now(),"agent_id":ORCHESTRATOR,
        "kind":{"type":"inbox.delivered","item":{"seq":seq,"kind":{
            "kind":"external_trigger","source":"github","operator":operator,
            "payload":{"operator":true,"repository":"o/r","number":1,"requester":"octocat","text":command}
        }}}
    })).unwrap()
    };
    pump::handle_orchestrator_event(&h.ctx, &event(1, true))
        .await
        .unwrap();
    assert!(merges(&h, &item).await.is_empty());
    assert!(
        triggers::held(&h.ctx)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e, TriggerEvent::ChatMergeRefused { .. }))
    );
    claim(&real_origin);
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let outcome = shipped_by(&h, &stage, "Changed it", json!({})).await;
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    let (_, head) = delivered(&h, &item).await;
    claim("https://github.com/o/r.git");
    gh.show(&head, "MERGEABLE", green());
    pump::handle_orchestrator_event(&h.ctx, &event(2, false))
        .await
        .unwrap();
    assert!(
        merge_calls(&gh).is_empty(),
        "payload flags alone grant no authority"
    );
    pump::handle_orchestrator_event(&h.ctx, &event(3, true))
        .await
        .unwrap();
    let [merge]: [MergeRequest; 1] = merges(&h, &item).await.try_into().unwrap();
    assert_eq!(
        merge.authority,
        MergeAuthority::Github {
            repository: "o/r".into(),
            number: 1,
            login: "octocat".into()
        }
    );
    assert!(matches!(merge.state, MergeState::Merged { .. }));
    assert_eq!(
        merge_calls(&gh),
        [format!(
            "pr merge {PR_ONE} --match-head-commit {head} --squash"
        )]
    );
}

/// The operator's whole-message `/merge` in the orchestrator's chat merges
/// in dedicated mode; in shared mode it is refused and said so.
#[tokio::test]
async fn the_aether_chat_merge_command_merges_only_in_dedicated_mode() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    chat(&h, 1, &format!("please /merge {}", item.id)).await;
    assert!(
        merges(&h, &item).await.is_empty(),
        "quoted text is no command"
    );
    chat(&h, 2, &format!("/merge {}", item.id)).await;
    let [merge] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(merge.authority, MergeAuthority::AetherChat);
    assert!(gh.merged() && matches!(merge.state, MergeState::Merged { .. }));
    assert!(matches!(merge_news(&h).await.as_slice(), [(true, _)]));

    let mut shared = Config::default();
    shared.aether.mode = AetherMode::Shared;
    let (h, gh) = delivering(shared).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    chat(&h, 1, &format!("/merge {}", item.id)).await;
    assert!(merges(&h, &item).await.is_empty() && !gh.merged());
    let held = triggers::held(&h.ctx).await.unwrap();
    assert!(
        held.iter().any(
            |e| matches!(e, TriggerEvent::ChatMergeRefused { text } if text.contains("shared"))
        ),
        "{held:?}"
    );
}

#[tokio::test]
async fn archon_merge_is_the_merge_command_for_the_operator_only() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    h.ctx
        .store
        .lock()
        .await
        .kv_set(keys::ORCHESTRATOR_AGENT, ORCHESTRATOR)
        .unwrap();
    let event = message_from(
        1,
        &format!("/archon merge {}", item.id),
        json!({"actor":"agent", "id":"agt_other"}),
    );
    pump::handle_orchestrator_event(&h.ctx, &event)
        .await
        .unwrap();
    assert!(merges(&h, &item).await.is_empty() && !gh.merged());

    chat(&h, 2, &format!("/archon merge {}", item.id)).await;
    let [merge] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(merge.authority, MergeAuthority::AetherChat);
    assert!(gh.merged() && matches!(merge.state, MergeState::Merged { .. }));
}

/// A merge sent while GitHub could not be read back stays `merging`; after
/// a restart the coordination pass settles it from what GitHub shows.
#[tokio::test]
async fn an_interrupted_merge_is_settled_from_github() {
    let (mut h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    let asked = merge_of(h.orchestrator(request_merge(&item)).await);
    let MergeAuthority::Confirmed { question, .. } = asked.authority else {
        panic!("confirmation")
    };
    gh.lose_next_merge_reply();
    h.cli(Request::Answer {
        chat: false,
        question: question.clone(),
        text: "yes".into(),
    })
    .await
    .unwrap();
    let requests = merges(&h, &item).await;
    assert_eq!(
        requests[0].state,
        MergeState::Merging,
        "not known to have merged"
    );
    assert_eq!(
        h.ctx
            .store
            .lock()
            .await
            .question(&question)
            .unwrap()
            .answer
            .as_deref(),
        Some("yes")
    );
    let refused = h.cli(merge_item(&item, false)).await;
    assert!(
        matches!(refused, Err(ArchonError::InvalidTransition(_))),
        "{refused:?}"
    );

    h.restart();
    coordination::reconcile(&h.ctx).await.unwrap();
    let [merge] = merges(&h, &item).await.try_into().expect("one request");
    let MergeState::Merged { method, .. } = &merge.state else {
        panic!("settled as merged: {merge:?}")
    };
    assert!(method.contains("confirmed later"), "{method}");
    assert_eq!(merge_calls(&gh).len(), 1, "never sent twice");
}

/// A grant covers no red checks and ends when it is turned off: either way
/// the operator is asked and nothing is sent to GitHub.
#[tokio::test]
async fn a_grant_asks_for_red_checks_and_not_at_all_once_off() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", red());
    let red_checks = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(red_checks.state, MergeState::AwaitingOperator);
    assert!(matches!(
        red_checks.authority,
        MergeAuthority::Confirmed { .. }
    ));
    assert!(merge_calls(&gh).is_empty() && !gh.merged());

    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    set_grant(&h, false).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", green());
    let revoked = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(revoked.state, MergeState::AwaitingOperator);
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

/// A head that moved after the operator was asked is refused on `yes`:
/// what merges is what was reviewed, never what was pushed since.
#[tokio::test]
async fn a_head_moved_after_the_question_is_refused_on_yes() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    let asked = merge_of(h.orchestrator(request_merge(&item)).await);
    let MergeAuthority::Confirmed { question, .. } = asked.authority else {
        panic!("asked: {asked:?}")
    };
    gh.show(&"0".repeat(40), "MERGEABLE", green());
    let yes = Request::Answer {
        chat: false,
        question,
        text: "yes".into(),
    };
    h.cli(yes).await.expect("the answer is recorded");
    let [merge] = merges(&h, &item).await.try_into().expect("one request");
    let MergeState::Refused { reason } = &merge.state else {
        panic!("refused: {merge:?}")
    };
    assert!(reason.to_lowercase().contains("head"), "{reason}");
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
    let news = merge_news(&h).await;
    assert!(matches!(news.as_slice(), [(false, _)]), "{news:?}");
}

/// A confirmation asked at an older head is closed as superseded when a
/// fresh request finds the pull request green at the delivered head.
#[tokio::test]
async fn a_request_at_a_new_head_supersedes_the_stale_confirmation() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    let asked = merge_of(h.orchestrator(request_merge(&item)).await);
    let MergeAuthority::Confirmed { question, .. } = asked.authority.clone() else {
        panic!("asked: {asked:?}")
    };
    let mut stale = asked;
    stale.head = "a".repeat(40);
    h.ctx.store.lock().await.update_merge(&stale).unwrap();

    let fresh = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(fresh.state, MergeState::AwaitingOperator);
    assert_eq!(fresh.head, head);
    let all = merges(&h, &item).await;
    assert_eq!(all.len(), 2, "{all:?}");
    let old = all
        .iter()
        .find(|m| m.id == stale.id)
        .expect("the stale one");
    let MergeState::Refused { reason } = &old.state else {
        panic!("closed: {old:?}")
    };
    assert!(
        reason.contains("superseded") && reason.contains("aaaaaaa"),
        "{reason}"
    );
    let closed = h.ctx.store.lock().await.question(&question).unwrap();
    assert!(!closed.is_open());
    assert!(!gh.merged());
}

/// A stage agent and the agents it delegated to cannot request a merge,
/// even of an item that could merge.
#[tokio::test]
async fn a_stage_agent_or_its_child_cannot_request_a_merge() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    enqueue(&h, "adaptive", ReviewLevel::Standard).await;
    h.dispatch().await;
    let lead = h
        .running()
        .await
        .remove(0)
        .aether_agent
        .expect("a lead agent");
    let child = h.backend.delegate(&lead, REVIEW_MODEL);
    for caller in [lead, child] {
        let request = Request::ForAgent {
            caller: caller.clone(),
            request: Box::new(request_merge(&item)),
        };
        let refused = handlers::handle(&h.ctx, ClientKind::ArchonMcp, request).await;
        assert!(
            matches!(refused, Err(ArchonError::Forbidden(_))),
            "{caller}: {refused:?}"
        );
    }
    assert!(merges(&h, &item).await.is_empty());
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

#[tokio::test]
async fn queued_merges_stay_open_until_github_merges_or_closes_them() {
    for actor in [Some("actual-merger"), None] {
        let (mut h, gh) = delivering(Config::default()).await;
        let (item, head) = shipped(&h).await;
        gh.show(&head, "MERGEABLE", green());
        gh.queue_next_merge();
        let request = merge_of(h.cli(merge_item(&item, false)).await);
        assert_eq!(request.state, MergeState::Queued);
        assert!(merge_news(&h).await.is_empty());
        h.restart();
        coordination::reconcile(&h.ctx).await.unwrap();
        assert_eq!(merges(&h, &item).await[0].state, MergeState::Queued);
        assert!(h.cli(merge_item(&item, false)).await.is_err());
        gh.finish_queued(actor);
        coordination::reconcile(&h.ctx).await.unwrap();
        let requests = merges(&h, &item).await;
        match (&requests[0].state, actor) {
            (
                MergeState::Merged {
                    actor: actual,
                    observed: true,
                    ..
                },
                Some(expected),
            ) => {
                assert_eq!(actual.as_deref(), Some(expected));
                let notes = h.notices(NotificationKind::Outcome).await;
                assert!(notes.iter().any(|n| n.contains("merged by actual-merger")));
            }
            (MergeState::Refused { .. }, None) => {}
            other => panic!("unexpected settlement: {other:?}"),
        }
        assert_eq!(merge_calls(&gh).len(), 1);
        assert_eq!(merge_news(&h).await.len(), 1);
    }
}

#[tokio::test]
async fn non_user_aether_messages_cannot_merge() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    h.ctx
        .store
        .lock()
        .await
        .kv_set(keys::ORCHESTRATOR_AGENT, ORCHESTRATOR)
        .unwrap();
    for (seq, actor) in [
        (1, json!({"actor":"agent", "id":"agt_other"})),
        (2, json!({"actor":"system"})),
    ] {
        let event = message_from(seq, &format!("/merge {}", item.id), actor);
        pump::handle_orchestrator_event(&h.ctx, &event)
            .await
            .unwrap();
    }
    assert!(merges(&h, &item).await.is_empty());
    assert!(merge_calls(&gh).is_empty());
}

#[tokio::test]
async fn changing_grant_merge_posts_the_feed_note() {
    let (h, _) = delivering(Config::default()).await;
    set_grant(&h, true).await;
    set_grant(&h, false).await;
    let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
    for expected in ["grant_merge is on", "grant_merge is off"] {
        assert!(feed.iter().any(|entry| matches!(&entry.entry, FeedEntry::System { text } if text.contains(expected))), "{expected}: {feed:?}");
    }
}

/// Under a grant, pending checks with none failed ask nothing: the request
/// waits, then merges once the pass sees every check green.
#[tokio::test]
async fn a_grant_waits_for_pending_checks_then_merges() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", pending());
    let waiting = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(waiting.state, MergeState::AwaitingChecks);
    assert_eq!(waiting.authority, MergeAuthority::Grant);
    assert!(h.ctx.store.lock().await.questions(true).unwrap().is_empty());

    coordination::reconcile(&h.ctx).await.unwrap();
    let [still] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(still.state, MergeState::AwaitingChecks, "still pending");
    assert!(merge_calls(&gh).is_empty() && !gh.merged());

    gh.show(&head, "MERGEABLE", green());
    coordination::reconcile(&h.ctx).await.unwrap();
    let [done] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(done.state.kind(), "merged", "{done:?}");
    assert!(gh.merged());
    let news = merge_news(&h).await;
    assert!(
        matches!(news.as_slice(), [(true, text)] if text.contains("waited")),
        "{news:?}"
    );
}

/// A check that fails while waiting asks the operator instead of merging.
#[tokio::test]
async fn a_waiting_grant_asks_when_a_check_fails() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", pending());
    merge_of(h.orchestrator(request_merge(&item)).await);
    gh.show(&head, "MERGEABLE", red());
    coordination::reconcile(&h.ctx).await.unwrap();
    let all = merges(&h, &item).await;
    let states: Vec<_> = all.iter().map(|m| m.state.kind()).collect();
    assert_eq!(states, ["refused", "awaiting_operator"], "{all:?}");
    assert!(matches!(all[1].authority, MergeAuthority::Confirmed { .. }));
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

/// A listed base merges at once under a grant, pending checks or not, and
/// says so; a base not listed still waits.
#[tokio::test]
async fn a_waived_base_merges_at_once_and_another_waits() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    waive_checks(&h, &["main"]).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", pending());
    let merge = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(merge.state.kind(), "merged", "{merge:?}");
    assert_eq!(merge.waived_by.as_deref(), Some("main"));
    assert!(gh.merged());
    let news = merge_news(&h).await;
    assert!(
        matches!(news.as_slice(), [(true, text)] if text.contains("merge_waive_checks")),
        "{news:?}"
    );

    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    waive_checks(&h, &["next"]).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", pending());
    let other = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(other.state, MergeState::AwaitingChecks);
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

/// The waiver never covers a pull request whose head moved after review,
/// and without a grant or a review it changes nothing.
#[tokio::test]
async fn a_waiver_needs_the_reviewed_head_a_grant_and_a_review() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    waive_checks(&h, &["main"]).await;
    let (item, _) = reviewed(&h).await;
    gh.show(&"0".repeat(40), "MERGEABLE", pending());
    let moved = h.orchestrator(request_merge(&item)).await;
    assert!(moved.is_err(), "{moved:?}");
    assert!(merge_calls(&gh).is_empty() && !gh.merged());

    let (h, gh) = delivering(Config::default()).await;
    waive_checks(&h, &["main"]).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", pending());
    let ungranted = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(ungranted.state, MergeState::AwaitingOperator);

    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    waive_checks(&h, &["main"]).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", pending());
    let unreviewed = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(unreviewed.state, MergeState::AwaitingOperator);
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

/// Turning the grant off while a request waits makes the pass ask, not merge.
#[tokio::test]
async fn a_waiting_grant_asks_once_the_grant_is_off() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", pending());
    merge_of(h.orchestrator(request_merge(&item)).await);
    set_grant(&h, false).await;
    gh.show(&head, "MERGEABLE", green());
    coordination::reconcile(&h.ctx).await.unwrap();
    let states: Vec<_> = merges(&h, &item)
        .await
        .iter()
        .map(|m| m.state.kind())
        .collect();
    assert_eq!(states, ["refused", "awaiting_operator"]);
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

/// The operator's own merge takes over a request still waiting for checks.
#[tokio::test]
async fn the_operators_merge_replaces_a_waiting_grant() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "MERGEABLE", pending());
    merge_of(h.orchestrator(request_merge(&item)).await);
    let reply = h.cli(merge_item(&item, true)).await;
    let merge = merge_of(reply);
    assert_eq!(merge.state.kind(), "merged", "{merge:?}");
    assert!(gh.merged());
}

/// Makes the one request of `item` look `minutes` old, as if it were asked,
/// and began waiting for GitHub (if it does), that long ago.
async fn age_request(h: &Harness, item: &QueueItem, minutes: i64) {
    let then = aged(minutes);
    let db = rusqlite::Connection::open(&h.ctx.paths.store).unwrap();
    let changed = db
        .execute(
            "UPDATE merge_requests SET requested_at = ?1, updated_at = ?1,
             data = json_set(data, '$.requested_at', ?1, '$.updated_at', ?1) WHERE item = ?2",
            [then.as_str(), item.id.as_str()],
        )
        .unwrap();
    assert_eq!(changed, 1, "one request aged");
    age_wait(h, item, minutes).await;
}

/// Makes the wait for GitHub begin, and be last read, `minutes` ago.
async fn age_wait(h: &Harness, item: &QueueItem, minutes: i64) {
    let then = aged(minutes);
    let db = rusqlite::Connection::open(&h.ctx.paths.store).unwrap();
    db.execute(
        "UPDATE merge_requests SET updated_at = ?1,
         data = json_set(data, '$.updated_at', ?1, '$.mergeability_wait_since', ?1)
         WHERE item = ?2 AND json_extract(data, '$.mergeability_wait_since') IS NOT NULL",
        [then.as_str(), item.id.as_str()],
    )
    .unwrap();
}

fn aged(minutes: i64) -> String {
    let then = archon_core::now() - chrono::TimeDelta::minutes(minutes);
    then.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// While GitHub computes mergeability a confirmed request keeps its
/// confirmation, says what it waits for, and merges once GitHub decides.
#[tokio::test]
async fn a_confirmed_request_survives_the_wait_for_mergeability_and_merges() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, head) = shipped(&h).await;
    gh.show(&head, "MERGEABLE", green());
    let asked = merge_of(h.orchestrator(request_merge(&item)).await);
    let MergeAuthority::Confirmed { question, .. } = asked.authority.clone() else {
        panic!("confirmed by a question: {asked:?}")
    };

    age_request(&h, &item, 120).await;
    gh.show(&head, "UNKNOWN", green());
    let yes = Request::Answer {
        chat: false,
        question,
        text: "yes".into(),
    };
    h.cli(yes).await.expect("the operator confirms");
    let [waiting] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(waiting.state, MergeState::AwaitingMergeability);
    assert!(matches!(
        waiting.authority,
        MergeAuthority::Confirmed { .. }
    ));
    assert!(
        waiting.line().contains("mergeability check"),
        "{}",
        waiting.line()
    );
    assert!(!gh.merged());

    age_wait(&h, &item, 1).await;
    coordination::poll_mergeability(&h.ctx).await.unwrap();
    let [still] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(
        still.state,
        MergeState::AwaitingMergeability,
        "still unknown"
    );

    gh.show(&head, "MERGEABLE", green());
    age_wait(&h, &item, 1).await;
    coordination::poll_mergeability(&h.ctx).await.unwrap();
    let [done] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(done.state.kind(), "merged", "{done:?}");
    assert!(gh.merged());
    let questions = h.ctx.store.lock().await.questions(true).unwrap();
    assert!(questions.is_empty(), "never asked again: {questions:?}");
}

/// Under a grant an undecided mergeability asks nothing, and a conflict found
/// while waiting refuses the request.
#[tokio::test]
async fn a_grant_waits_for_mergeability_then_refuses_a_conflict() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "UNKNOWN", green());
    let waiting = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(waiting.state, MergeState::AwaitingMergeability);
    assert!(h.ctx.store.lock().await.questions(true).unwrap().is_empty());

    gh.show(&head, "CONFLICTING", green());
    age_request(&h, &item, 1).await;
    coordination::poll_mergeability(&h.ctx).await.unwrap();
    let [refused] = merges(&h, &item).await.try_into().expect("one request");
    let MergeState::Refused { reason } = &refused.state else {
        panic!("refused: {refused:?}")
    };
    assert!(reason.contains("conflicts"), "{reason}");
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

/// Under a grant the request merges as soon as GitHub says it is mergeable.
#[tokio::test]
async fn a_grant_merges_once_github_decides_it_is_mergeable() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "UNKNOWN", green());
    let waiting = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(waiting.state, MergeState::AwaitingMergeability);

    gh.show(&head, "MERGEABLE", green());
    age_request(&h, &item, 1).await;
    coordination::poll_mergeability(&h.ctx).await.unwrap();
    let [done] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(done.state.kind(), "merged", "{done:?}");
    assert!(gh.merged());
    assert!(h.ctx.store.lock().await.questions(true).unwrap().is_empty());
}

/// A wait that outlasts the hour refuses and says what to do next.
#[tokio::test]
async fn a_wait_for_mergeability_that_times_out_says_what_to_do() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "UNKNOWN", green());
    merge_of(h.orchestrator(request_merge(&item)).await);

    age_request(&h, &item, 61).await;
    coordination::poll_mergeability(&h.ctx).await.unwrap();
    let [refused] = merges(&h, &item).await.try_into().expect("one request");
    let MergeState::Refused { reason } = &refused.state else {
        panic!("refused: {refused:?}")
    };
    assert!(
        reason.contains("gh pr view") && reason.contains("ask for the merge again"),
        "{reason}"
    );
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

/// A wait stored before its start was recorded keeps its original start
/// through retries, so its hour still runs out.
#[tokio::test]
async fn a_legacy_wait_for_mergeability_keeps_its_start_and_times_out() {
    let (h, gh) = delivering(Config::default()).await;
    grant_merge(&h).await;
    let (item, head) = reviewed(&h).await;
    gh.show(&head, "UNKNOWN", green());
    merge_of(h.orchestrator(request_merge(&item)).await);
    age_request(&h, &item, 30).await;
    let db = rusqlite::Connection::open(&h.ctx.paths.store).unwrap();
    db.execute(
        "UPDATE merge_requests SET data = json_remove(data, '$.mergeability_wait_since')",
        [],
    )
    .unwrap();

    coordination::poll_mergeability(&h.ctx).await.unwrap();
    let [kept] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(kept.state, MergeState::AwaitingMergeability);
    let since = kept.mergeability_wait_since.expect("the start is kept");
    assert!(
        archon_core::now() - since >= chrono::TimeDelta::minutes(29),
        "{since}"
    );

    age_request(&h, &item, 61).await;
    db.execute(
        "UPDATE merge_requests SET data = json_remove(data, '$.mergeability_wait_since')",
        [],
    )
    .unwrap();
    coordination::poll_mergeability(&h.ctx).await.unwrap();
    let [done] = merges(&h, &item).await.try_into().expect("one request");
    assert!(matches!(done.state, MergeState::Refused { .. }), "{done:?}");
}
