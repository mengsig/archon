//! Team roles working together: the roster each running role is sent once
//! per version, again after a role is replaced and never twice across a
//! restart, and the live siblings of a role that failed, cancelled at once
//! with their agents stopped even when Aether is briefly unreachable.

mod common;

use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::queue::QueueState;
use archon_core::run::{PipelineRun, StageOutcome, StageRun, StageState};
use archon_daemon::executor;
use common::team::{
    TEAM_PIPELINE, TEAM_TOML, branch, role_of, start_plan, start_team, team_request, worker,
};
use common::{Harness, completed};

/// The running role `role`'s stage run.
async fn running_role(h: &Harness, role: &str) -> StageRun {
    let running = h.running().await;
    let found = running
        .iter()
        .find(|s| s.stage_id == "implement" && role_of(s) == role);
    found
        .cloned()
        .unwrap_or_else(|| panic!("no running {role}: {running:?}"))
}

/// The roster line of `role`, as a message names it.
fn roster_line(run: &PipelineRun, role: &str, stage: &StageRun, attempt: u32) -> String {
    let agent = stage.aether_agent.as_ref().expect("an agent");
    format!(
        "- `{role}`: agent `{agent}`, worktree `{}`, branch `{}`",
        worker(run, role).display(),
        branch(run, role, attempt)
    )
}

#[tokio::test]
async fn each_running_role_gets_the_roster_once() {
    let h = Harness::new().await;
    let (_, run) = start_team(&h, "two roles talk").await;
    let api = running_role(&h, "api").await;
    let ui = running_role(&h, "ui").await;
    let messages = h.backend.messages();
    assert_eq!(messages.len(), 2, "{messages:?}");
    for (stage, role) in [(&api, "api"), (&ui, "ui")] {
        let agent = stage.aether_agent.clone().expect("an agent");
        let (_, text) = messages.iter().find(|(to, _)| *to == agent).expect("sent");
        assert!(text.starts_with("Archon team roster v1 for team stage `implement` attempt 1"));
        assert!(text.contains(&format!("You are role `{role}`")), "{text}");
        assert!(text.contains(&roster_line(&run, "api", &api, 1)), "{text}");
        assert!(text.contains(&roster_line(&run, "ui", &ui, 1)), "{text}");
        assert!(text.contains("`send_message`"), "{text}");
    }
    let calls = h.backend.message_calls().len();
    h.dispatch().await;
    h.dispatch().await;
    assert_eq!(h.backend.message_calls().len(), calls, "nothing is re-sent");
}

#[tokio::test]
async fn a_restart_does_not_send_the_roster_again() {
    let mut h = Harness::new().await;
    start_team(&h, "restart after the roster").await;
    let calls = h.backend.message_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    assert_eq!(h.backend.message_calls(), calls, "no second send");
    assert_eq!(h.backend.messages().len(), 2);
}

#[tokio::test]
async fn an_unanswered_roster_send_is_retried_under_its_key() {
    let h = Harness::new().await;
    h.backend
        .fail_next_message(ArchonError::Unavailable("aether restarting".into()));
    start_team(&h, "flaky link").await;
    assert_eq!(h.backend.messages().len(), 1, "one send was lost");
    h.dispatch().await;
    let calls = h.backend.message_calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], calls[2], "the retry repeats the key");
    assert_eq!(h.backend.messages().len(), 2, "each role has it once");
}

#[tokio::test]
async fn a_replaced_role_brings_a_new_roster_version_to_live_peers() {
    let h = Harness::new().await;
    let (_, run) = start_team(&h, "role replaced").await;
    let api = running_role(&h, "api").await;
    let ui = running_role(&h, "ui").await;
    let failed = StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    };
    let agent = ui.aether_agent.clone().expect("running");
    executor::stage_settled(&h.ctx, &agent, failed)
        .await
        .expect("settles");
    h.dispatch().await;

    let new_ui = running_role(&h, "ui").await;
    assert_ne!(new_ui.id, ui.id, "ui was replaced");
    let messages = h.backend.messages();
    assert_eq!(messages.len(), 4, "v1 twice, then v2 twice: {messages:?}");
    let v2: Vec<&(_, String)> = messages[2..].iter().collect();
    let recipients: Vec<_> = v2.iter().map(|(to, _)| to.clone()).collect();
    assert!(recipients.contains(api.aether_agent.as_ref().expect("api")));
    assert!(recipients.contains(new_ui.aether_agent.as_ref().expect("new ui")));
    for (_, text) in v2 {
        assert!(text.starts_with("Archon team roster v2 "), "{text}");
        assert!(
            text.contains(&roster_line(&run, "ui", &new_ui, 1)),
            "{text}"
        );
    }
    h.dispatch().await;
    assert_eq!(h.backend.messages().len(), 4, "v2 is sent once");
}

#[tokio::test]
async fn a_role_that_finished_is_not_sent_a_new_version() {
    let h = Harness::new().await;
    start_team(&h, "api done early").await;
    let api = running_role(&h, "api").await;
    let ui = running_role(&h, "ui").await;
    let api_agent = api.aether_agent.clone().expect("running");
    executor::stage_settled(&h.ctx, &api_agent, completed("api"))
        .await
        .expect("settles");
    let failed = StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    };
    let ui_agent = ui.aether_agent.clone().expect("running");
    executor::stage_settled(&h.ctx, &ui_agent, failed)
        .await
        .expect("settles");
    h.dispatch().await;
    let messages = h.backend.messages();
    assert_eq!(messages.len(), 3, "{messages:?}");
    let new_ui = running_role(&h, "ui").await;
    assert_eq!(Some(&messages[2].0), new_ui.aether_agent.as_ref());
}

/// A role that fails stops the team attempt: its live sibling is cancelled
/// at once with the reason recorded, and `plan` runs again with feedback
/// naming the failed role.
#[tokio::test]
async fn a_failed_role_cancels_its_live_sibling() {
    let h = Harness::new().await;
    let (item, _) = start_team(&h, "one role fails").await;
    let running = h.running().await;
    let ui = running.iter().find(|s| role_of(s) == "ui").expect("ui");
    let api = running.iter().find(|s| role_of(s) == "api").expect("api");
    let failed = StageOutcome::Failed {
        error: "the ui build is broken".into(),
        infra: false,
    };
    let ui_agent = ui.aether_agent.clone().expect("running");
    executor::stage_settled(&h.ctx, &ui_agent, failed)
        .await
        .expect("settles");

    let api_agent = api.aether_agent.clone().expect("running");
    assert_eq!(h.backend.cancelled(), vec![api_agent.clone()]);
    let stages = h.stages(&item.id).await;
    let api_now = stages.iter().find(|s| s.id == api.id).expect("api");
    assert_eq!(api_now.state, StageState::Cancelled);
    let reason = api_now.summary.as_deref().expect("the reason is recorded");
    assert!(reason.contains("role `ui` failed"), "{reason}");
    let notices = h.notices(NotificationKind::Progress).await;
    let notice = notices.iter().find(|n| n.contains("role(s) `api`"));
    assert!(
        notice.is_some_and(|n| n.contains("role `ui` failed")),
        "{notices:?}"
    );

    // The cancelled agent's own settle arrives later and changes nothing.
    executor::stage_settled(&h.ctx, &api_agent, completed("api"))
        .await
        .expect("a late settle is ignored");
    h.dispatch().await;
    let replan = h.running().await;
    assert_eq!(replan.len(), 1, "{replan:?}");
    assert_eq!(
        (replan[0].stage_id.as_str(), replan[0].attempt),
        ("plan", 2)
    );
    let objective = h.backend.resumed_with(&replan[0]);
    assert!(objective.contains("role `ui` failed"), "{objective}");
    assert!(objective.contains("the ui build is broken"), "{objective}");
    assert_eq!(h.backend.cancelled().len(), 1, "cancelled once");
}

/// A role that times out on its last loop fails the run and stops its
/// sibling; the run's error names the role.
#[tokio::test]
async fn a_timed_out_role_on_its_last_loop_fails_the_run() {
    let h = Harness::new().await;
    let (item, _) = start_team(&h, "slow role").await;
    h.settle_running(|s| StageOutcome::Failed {
        error: format!("{} gave up", role_of(s)),
        infra: false,
    })
    .await;
    h.dispatch().await;
    h.settle_running(|_| completed("planned again")).await;
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(running.len(), 2, "attempt 2 of both roles: {running:?}");
    let api = running.iter().find(|s| role_of(s) == "api").expect("api");
    let ui = running.iter().find(|s| role_of(s) == "ui").expect("ui");
    let agent = api.aether_agent.clone().expect("running");
    executor::stage_settled(&h.ctx, &agent, StageOutcome::TimedOut)
        .await
        .expect("settles");

    match h.state(&item.id).await {
        QueueState::Failed { error, .. } => {
            assert!(error.contains("role `api` timed out"), "{error}");
            assert!(error.contains("role `ui` was cancelled"), "{error}");
        }
        other => panic!("expected a failed item, got {other:?}"),
    }
    assert!(h.running().await.is_empty(), "the sibling is stopped");
    let stages = h.stages(&item.id).await;
    let ui_now = stages.iter().find(|s| s.id == ui.id).expect("ui");
    assert_eq!(ui_now.state, StageState::Cancelled);
    let reason = ui_now.summary.as_deref().expect("the reason is recorded");
    assert!(
        reason.contains("cancelled because role `api` timed out"),
        "not the finish's generic reason: {reason}"
    );
    let notices = h.notices(NotificationKind::Progress).await;
    assert!(
        notices
            .iter()
            .any(|n| n.contains("role(s) `ui`") && n.contains("role `api` timed out")),
        "{notices:?}"
    );
    let ui_agent = ui.aether_agent.clone().expect("running");
    h.dispatch().await;
    let stops = h.backend.cancelled();
    assert!(stops.contains(&ui_agent), "{stops:?}");
}

/// The daemon died after a role's failure was persisted but before its
/// sibling was cancelled: recovery cancels it and the sweep stops its
/// agent, once.
#[tokio::test]
async fn recovery_cancels_the_sibling_of_a_role_that_failed_before_a_crash() {
    let mut h = Harness::new().await;
    let (item, _) = start_team(&h, "crash after a failure").await;
    let running = h.running().await;
    let api = running.iter().find(|s| role_of(s) == "api").expect("api");
    let ui = running.iter().find(|s| role_of(s) == "ui").expect("ui");
    {
        let mut store = h.ctx.store.lock().await;
        let mut failed = store.stage_run(&ui.id).expect("ui");
        failed.state = StageState::Failed {
            error: "crashed mid-build".into(),
        };
        failed.finished_at = Some(archon_core::now());
        store.upsert_stage_run(&failed).expect("persist");
    }
    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    let agent = api.aether_agent.clone().expect("running");
    assert_eq!(h.backend.cancelled(), vec![agent]);
    h.restart();
    executor::recover(&h.ctx).await.expect("recover again");
    h.dispatch().await;
    assert_eq!(h.backend.cancelled().len(), 1, "no second cancel");
    let stages = h.stages(&item.id).await;
    let api_now = stages.iter().find(|s| s.id == api.id).expect("api");
    assert_eq!(api_now.state, StageState::Cancelled);
    let replan = h.running().await;
    assert_eq!(
        (replan[0].stage_id.as_str(), replan[0].attempt),
        ("plan", 2)
    );
}

#[tokio::test]
async fn a_sibling_stop_aether_missed_is_retried_by_the_sweep() {
    let h = Harness::new().await;
    start_team(&h, "stop retried").await;
    let api = running_role(&h, "api").await;
    let ui = running_role(&h, "ui").await;
    h.backend
        .fail_next_cancel(ArchonError::Unavailable("aether restarting".into()));
    let failed = StageOutcome::Failed {
        error: "gave up".into(),
        infra: false,
    };
    let ui_agent = ui.aether_agent.clone().expect("running");
    executor::stage_settled(&h.ctx, &ui_agent, failed)
        .await
        .expect("settles");
    assert!(
        h.backend.cancelled().is_empty(),
        "the first stop was missed"
    );
    let key = format!("sibling_stop:{}", api.id);
    assert!(h.kv(&key).await.is_some(), "the stop is owed");

    h.dispatch().await;
    let api_agent = api.aether_agent.clone().expect("running");
    assert_eq!(h.backend.cancelled(), vec![api_agent]);
    assert_eq!(h.kv(&key).await, None, "the stop is done");
    h.dispatch().await;
    assert_eq!(h.backend.cancelled().len(), 1, "stopped once");
}

/// The refusal of enqueueing the two-role team under `h`'s caps.
async fn team_refusal(h: &Harness) -> String {
    h.add_pipeline(TEAM_PIPELINE, TEAM_TOML);
    match h.cli(team_request("too big a team")).await {
        Err(ArchonError::Invalid(why)) => why,
        other => panic!("a team over the caps was not refused: {other:?}"),
    }
}

#[tokio::test]
async fn a_team_bigger_than_the_repository_cap_is_refused_at_enqueue() {
    let h = Harness::new().await;
    h.repo_config("[limits]\nmax_per_repo = 1\n");
    let why = team_refusal(&h).await;
    assert!(why.contains("team stage \"implement\""), "{why}");
    assert!(
        why.contains("2 roles") && why.contains("at most 1"),
        "{why}"
    );
    assert!(h.backend.requests().is_empty());
}

#[tokio::test]
async fn a_team_bigger_than_the_global_cap_is_refused_at_enqueue() {
    let mut config = Config::default();
    config.limits.max_parallel_agents = 1;
    let h = Harness::with_config(config).await;
    let why = team_refusal(&h).await;
    assert!(
        why.contains("2 roles") && why.contains("at most 1"),
        "{why}"
    );
}

#[tokio::test]
async fn a_team_that_fits_the_caps_exactly_is_enqueued() {
    let mut config = Config::default();
    config.limits.max_parallel_agents = 2;
    let h = Harness::with_config(config).await;
    let (_, run) = start_team(&h, "exactly two agents").await;
    assert_eq!(h.running().await.len(), 2, "both roles of {} run", run.id);
}

/// The planning team stage over a cap lowered after enqueue: one `Blocker`
/// naming the stage and the cap, not repeated while it still waits, and
/// the record cleared once the cap is raised and the team starts.
#[tokio::test]
async fn a_team_over_a_lowered_cap_is_announced_once() {
    let h = Harness::new().await;
    let (item, run) = start_plan(&h, "caps lowered under a team").await;
    h.repo_config("[limits]\nmax_per_repo = 1\n");
    h.settle_running(|_| completed("split into api and ui"))
        .await;
    h.dispatch().await;
    h.dispatch().await;
    assert!(h.running().await.is_empty(), "the team waits whole");
    let blockers = h.notices(NotificationKind::Blocker).await;
    let over: Vec<&String> = blockers
        .iter()
        .filter(|b| b.contains("agents at once"))
        .collect();
    assert_eq!(over.len(), 1, "announced once: {blockers:?}");
    assert!(over[0].contains("team stage `implement`"), "{}", over[0]);
    assert!(
        over[0].contains("needs 2 agents") && over[0].contains("allow 1"),
        "{}",
        over[0]
    );
    assert!(over[0].contains(item.id.as_str()), "{}", over[0]);
    let marker = h.kv("dispatch_never_fits").await.unwrap_or_default();
    assert!(marker.contains(run.id.as_str()), "{marker}");

    h.repo_config("[limits]\nmax_per_repo = 2\n");
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2, "both roles start");
    assert_eq!(h.kv("dispatch_never_fits").await.as_deref(), Some(""));
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(
        blockers
            .iter()
            .filter(|b| b.contains("agents at once"))
            .count(),
        1
    );
}
